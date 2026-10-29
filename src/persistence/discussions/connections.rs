//! `DiscussionStore` operations on the per-organization Discord discussion connection.

use rusqlite::TransactionBehavior;
use rusqlite::params;

use super::{
    DiscussionStore,
    connection_rows::{
        DiscussionConnectionRow, connection_row, connection_row_by_id, delivery_url_for_connection,
        has_bound_connection_authority, new_connection_id, notification_webhook_is_eligible,
        org_exists, valid_discord_id,
    },
    inputs::{CreateDiscussionConnection, CreateNotificationThreadConnection},
    internal,
    support::{append_audit_then_commit, require_nonempty, trimmed},
    types::{DiscussionConnectionDelivery, DiscussionConnectionSummary},
};
use crate::{
    error::AppError,
    model::OrgId,
    persistence::{
        db,
        webhooks::{INVALID_URL_MESSAGE, MAX_LABEL_UTF16, is_discord_webhook_url, truncate_utf16},
    },
    security::audit::{AuditEvent, MutationAudit, append_in_transaction, mutate_in_transaction},
};

impl DiscussionStore {
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
}
