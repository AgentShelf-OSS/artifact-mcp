use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};

use crate::{
    error::AppError,
    integrations::discord_inbound::{
        DiscordMessage, DiscordRestPort, IgnoreReason, InboundEvent, InboundEventInbox,
        InboundFeedback, InboundIntegrationState, InboundProcessor, InboundResult, MemoryInbox,
        RejectReason, RestError, ThreadDegradedReason, TwoWayThreadAuthorization,
    },
    model::{ArtifactId, FeedbackAuthor, FeedbackId, OrgId},
};

use super::common::internal;

fn artifact_exists(
    conn: &Connection,
    artifact: &ArtifactId,
    org: &OrgId,
) -> Result<bool, AppError> {
    conn.query_row(
        "SELECT 1 FROM artifacts WHERE id=?1 AND org=?2",
        params![artifact.0, org.0],
        |_| Ok(()),
    )
    .optional()
    .map(|row| row.is_some())
    .map_err(internal)
}

fn connected_inbound_destination(
    conn: &Connection,
    artifact: &ArtifactId,
    org: &OrgId,
) -> Result<bool, AppError> {
    conn.query_row(
        "SELECT EXISTS( \
           SELECT 1 FROM artifact_discussions d \
           JOIN org_discord_discussion_connections c \
             ON c.id=d.connection_id AND c.org=d.org \
           JOIN org_discord_threading_policies p ON p.org=d.org \
          WHERE d.artifact_id=?1 AND d.org=?2 AND d.provider='discord' \
            AND d.mode='discord_mirror' AND d.state='connected' \
            AND d.thread_id IS NOT NULL AND d.thread_id <> '' \
            AND c.strategy='notification_thread' AND c.guild_id IS NOT NULL \
            AND c.guild_id <> '' AND c.notification_provider_webhook_id IS NOT NULL \
            AND c.notification_provider_webhook_id <> '' AND p.outbound_enabled=1 \
            AND EXISTS (SELECT 1 FROM discord_gateway_sessions g \
                         WHERE g.org=d.org AND g.health='ready') \
         )",
        params![artifact.0, org.0],
        |row| row.get(0),
    )
    .map_err(internal)
}

pub(super) fn apply_in_transaction(
    conn: &mut Connection,
    event: &InboundEvent,
) -> Result<InboundResult, AppError> {
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(internal)?;

    if event_exists(&tx, event)? {
        return Ok(InboundResult::Duplicate);
    }

    let binding = authorization_for_event(&tx, &event.org, &event.guild_id, &event.thread_id)?;
    if locally_moderated_message(&tx, event)? {
        let result = InboundResult::Ignored(
            crate::integrations::discord_inbound::IgnoreReason::UnknownMessage,
        );
        record_event(&tx, event, result)?;
        tx.commit().map_err(internal)?;
        return Ok(result);
    }
    let mut inbox = MemoryInbox::default();
    if let Some(binding) = binding.as_ref() {
        load_relevant_feedback(&tx, binding, event, &mut inbox)?;
    }

    let processor = InboundProcessor::new(InboundIntegrationState {
        enabled: true,
        health: crate::integrations::discord_inbound::GatewayHealth::Ready,
    });
    let result = processor.process(binding.as_ref(), event.clone(), &mut inbox, &NoFetchRest);

    if result == InboundResult::Applied {
        let message_id = event
            .message
            .as_ref()
            .map(|message| message.id.as_str())
            .ok_or_else(|| {
                AppError::Validation("Discord event is missing a message.".to_owned())
            })?;
        let feedback = inbox.feedback.get(message_id).ok_or(AppError::Internal)?;
        persist_feedback(&tx, feedback, &event.thread_id)?;
    } else if let InboundResult::Degraded(reason) = result
        && let Some(binding) = binding.as_ref()
    {
        mark_policy_degraded(&tx, binding, reason)?;
    }

    record_event(&tx, event, result)?;
    tx.commit().map_err(internal)?;
    Ok(result)
}

fn exact_authorization(
    conn: &Connection,
    org: &OrgId,
    guild_id: &str,
    thread_id: &str,
) -> Result<Option<TwoWayThreadAuthorization>, AppError> {
    conn.query_row(
        "SELECT p.artifact_id, c.notification_provider_webhook_id \
           FROM artifact_discord_inbound_policies p \
           JOIN artifact_discussions d \
             ON d.artifact_id=p.artifact_id AND d.org=p.org \
           JOIN org_discord_discussion_connections c \
             ON c.id=d.connection_id AND c.org=d.org \
          WHERE p.org=?1 AND p.enabled=1 \
            AND d.provider='discord' AND d.mode='discord_mirror' \
            AND d.state='connected' AND d.thread_id=?3 \
            AND c.strategy='notification_thread' AND c.guild_id=?2 \
          LIMIT 1",
        params![org.0, guild_id, thread_id],
        |row| {
            Ok(TwoWayThreadAuthorization {
                org: org.clone(),
                artifact_id: ArtifactId(row.get(0)?),
                guild_id: guild_id.to_owned(),
                thread_id: thread_id.to_owned(),
                enabled: true,
                // Gateway's author.bot flag is authoritative. The value remains as a second
                // provider-adapter check when a future credential projection stores the bot id.
                configured_bot_user_id: String::new(),
                configured_webhook_id: row.get(1)?,
            })
        },
    )
    .optional()
    .map_err(internal)
}

fn authorization_for_event(
    conn: &Connection,
    org: &OrgId,
    guild_id: &str,
    thread_id: &str,
) -> Result<Option<TwoWayThreadAuthorization>, AppError> {
    if let Some(binding) = exact_authorization(conn, org, guild_id, thread_id)? {
        return Ok(Some(binding));
    }

    // A thread can be a valid mapping for another tenant in the same Discord guild. Resolve only
    // enough server-side state for the pure processor to emit a durable CrossTenant rejection.
    // The foreign artifact/provider identifiers never leave persistence or enter an error.
    conn.query_row(
        "SELECT p.org, p.artifact_id, c.notification_provider_webhook_id \
           FROM artifact_discord_inbound_policies p \
           JOIN artifact_discussions d \
             ON d.artifact_id=p.artifact_id AND d.org=p.org \
           JOIN org_discord_discussion_connections c \
             ON c.id=d.connection_id AND c.org=d.org \
          WHERE p.org<>?1 AND p.enabled=1 \
            AND d.provider='discord' AND d.mode='discord_mirror' \
            AND d.state='connected' AND d.thread_id=?3 \
            AND c.strategy='notification_thread' AND c.guild_id=?2 \
          LIMIT 1",
        params![org.0, guild_id, thread_id],
        |row| {
            Ok(TwoWayThreadAuthorization {
                org: OrgId(row.get(0)?),
                artifact_id: ArtifactId(row.get(1)?),
                guild_id: guild_id.to_owned(),
                thread_id: thread_id.to_owned(),
                enabled: true,
                configured_bot_user_id: String::new(),
                configured_webhook_id: row.get(2)?,
            })
        },
    )
    .optional()
    .map_err(internal)
}

fn locally_moderated_message(conn: &Connection, event: &InboundEvent) -> Result<bool, AppError> {
    let Some(message_id) = event.message.as_ref().map(|message| message.id.as_str()) else {
        return Ok(false);
    };
    conn.query_row(
        "SELECT 1 FROM discord_inbound_message_state \
          WHERE provider='discord' AND org=?1 AND external_message_id=?2 \
            AND feedback_id IS NULL",
        params![event.org.0, message_id],
        |_| Ok(()),
    )
    .optional()
    .map(|row| row.is_some())
    .map_err(internal)
}

fn event_exists(tx: &Transaction<'_>, event: &InboundEvent) -> Result<bool, AppError> {
    tx.query_row(
        "SELECT 1 FROM discord_inbound_events \
          WHERE provider='discord' AND org=?1 AND gateway_session_id=?2 AND event_id=?3 \
            AND result <> 'needs_fetch'",
        params![event.org.0, event.gateway_session_id, event.event_id],
        |_| Ok(()),
    )
    .optional()
    .map(|row| row.is_some())
    .map_err(internal)
}

fn load_relevant_feedback(
    tx: &Transaction<'_>,
    binding: &TwoWayThreadAuthorization,
    event: &InboundEvent,
    inbox: &mut MemoryInbox,
) -> Result<(), AppError> {
    let Some(message) = event.message.as_ref() else {
        return Ok(());
    };
    load_message(tx, binding, &message.id, inbox)?;
    if let Some(parent) = message.reply_to_message_id.as_deref() {
        load_message(tx, binding, parent, inbox)?;
    }
    Ok(())
}

fn load_message(
    tx: &Transaction<'_>,
    binding: &TwoWayThreadAuthorization,
    message_id: &str,
    inbox: &mut MemoryInbox,
) -> Result<(), AppError> {
    let row = tx
        .query_row(
            "SELECT s.feedback_id, f.parent_id, s.external_author_id, \
                    s.external_author_display, f.body, s.provider_version, \
                    s.external_created_at, s.external_edited_at, s.external_deleted_at \
               FROM discord_inbound_message_state s \
               JOIN feedback f \
                 ON f.id=s.feedback_id AND f.artifact_id=s.artifact_id AND f.org=s.org \
              WHERE s.provider='discord' AND s.org=?1 AND s.artifact_id=?2 \
                AND s.external_thread_id=?3 AND s.external_message_id=?4",
            params![
                binding.org.0,
                binding.artifact_id.0,
                binding.thread_id,
                message_id
            ],
            |row| {
                Ok(InboundFeedback {
                    id: FeedbackId(row.get(0)?),
                    artifact_id: binding.artifact_id.clone(),
                    org: binding.org.clone(),
                    parent_id: row.get::<_, Option<String>>(1)?.map(FeedbackId),
                    author: FeedbackAuthor::Discord {
                        external_author_id: row.get(2)?,
                        external_author_display: row.get(3)?,
                    },
                    body: row.get(4)?,
                    external_message_id: message_id.to_owned(),
                    provider_version: row.get(5)?,
                    external_created_at: row.get(6)?,
                    external_edited_at: row.get(7)?,
                    external_deleted_at: row.get(8)?,
                })
            },
        )
        .optional()
        .map_err(internal)?;
    if let Some(row) = row {
        inbox.put_feedback(row);
        return Ok(());
    }

    // A Discord reply may reference an Artifact-origin comment that PBI-079 mirrored outward.
    // Load only its canonical thread identity; this is a read-only parent correlation and can
    // never make that Artifact row look externally authored.
    let mirrored = tx
        .query_row(
            "SELECT f.id, f.parent_id, f.viewer_email, f.body \
               FROM discussion_message_links l \
               JOIN feedback f \
                 ON f.id=l.feedback_id AND f.artifact_id=l.artifact_id AND f.org=l.org \
              WHERE l.provider='discord' AND l.org=?1 AND l.artifact_id=?2 \
                AND l.external_thread_id=?3 AND l.external_message_id=?4 \
                AND l.source='artifact' AND l.state IN ('posted','local_deleted')",
            params![
                binding.org.0,
                binding.artifact_id.0,
                binding.thread_id,
                message_id
            ],
            |row| {
                let viewer_email = row.get::<_, Option<String>>(2)?.ok_or_else(|| {
                    rusqlite::Error::InvalidColumnType(
                        2,
                        "viewer_email".to_owned(),
                        rusqlite::types::Type::Null,
                    )
                })?;
                Ok(InboundFeedback {
                    id: FeedbackId(row.get(0)?),
                    artifact_id: binding.artifact_id.clone(),
                    org: binding.org.clone(),
                    parent_id: row.get::<_, Option<String>>(1)?.map(FeedbackId),
                    author: FeedbackAuthor::Artifact {
                        viewer_email: crate::model::EmailAddress(viewer_email),
                    },
                    body: row.get(3)?,
                    external_message_id: message_id.to_owned(),
                    provider_version: 0,
                    external_created_at: None,
                    external_edited_at: None,
                    external_deleted_at: None,
                })
            },
        )
        .optional()
        .map_err(internal)?;
    if let Some(row) = mirrored {
        inbox.put_feedback(row);
    }
    Ok(())
}

fn persist_feedback(
    tx: &Transaction<'_>,
    feedback: &InboundFeedback,
    thread_id: &str,
) -> Result<(), AppError> {
    let FeedbackAuthor::Discord {
        external_author_id,
        external_author_display,
    } = &feedback.author
    else {
        return Err(AppError::Internal);
    };
    let revision = feedback_artifact_revision(tx, feedback)?;
    if feedback_mapping_exists(tx, feedback)? {
        update_feedback_mapping(tx, feedback, thread_id, external_author_display)
    } else {
        insert_feedback_mapping(
            tx,
            feedback,
            thread_id,
            revision,
            external_author_id,
            external_author_display,
        )
    }
}

fn feedback_artifact_revision(
    tx: &Transaction<'_>,
    feedback: &InboundFeedback,
) -> Result<i64, AppError> {
    tx.query_row(
        "SELECT revision FROM artifacts WHERE id=?1 AND org=?2",
        params![feedback.artifact_id.0, feedback.org.0],
        |row| row.get(0),
    )
    .map_err(internal)
}

fn feedback_mapping_exists(
    tx: &Transaction<'_>,
    feedback: &InboundFeedback,
) -> Result<bool, AppError> {
    tx.query_row(
        "SELECT 1 FROM discord_inbound_message_state \
          WHERE provider='discord' AND org=?1 AND external_message_id=?2",
        params![feedback.org.0, feedback.external_message_id],
        |_| Ok(()),
    )
    .optional()
    .map(|row| row.is_some())
    .map_err(internal)
}

fn update_feedback_mapping(
    tx: &Transaction<'_>,
    feedback: &InboundFeedback,
    thread_id: &str,
    external_author_display: &str,
) -> Result<(), AppError> {
    tx.execute(
        "UPDATE feedback SET body=?1, external_edited_at=?2, external_deleted_at=?3 \
          WHERE id=?4 AND artifact_id=?5 AND org=?6 AND author_source='discord'",
        params![
            feedback.body,
            feedback.external_edited_at,
            feedback.external_deleted_at,
            feedback.id.0,
            feedback.artifact_id.0,
            feedback.org.0
        ],
    )
    .map_err(internal)?;
    tx.execute(
        "UPDATE discord_inbound_message_state \
            SET provider_version=?1, external_author_display=?2, external_edited_at=?3, \
                external_deleted_at=?4, updated_at=datetime('now') \
          WHERE provider='discord' AND org=?5 AND external_message_id=?6 \
            AND artifact_id=?7 AND external_thread_id=?8",
        params![
            feedback.provider_version,
            external_author_display,
            feedback.external_edited_at,
            feedback.external_deleted_at,
            feedback.org.0,
            feedback.external_message_id,
            feedback.artifact_id.0,
            thread_id
        ],
    )
    .map_err(internal)?;
    Ok(())
}

fn insert_feedback_mapping(
    tx: &Transaction<'_>,
    feedback: &InboundFeedback,
    thread_id: &str,
    revision: i64,
    external_author_id: &str,
    external_author_display: &str,
) -> Result<(), AppError> {
    tx.execute(
        "INSERT INTO feedback \
         (id, artifact_id, org, viewer_email, body, artifact_revision, parent_id, \
          author_source, external_author_id, external_author_display, external_created_at, \
          external_edited_at, external_deleted_at) \
         VALUES (?1, ?2, ?3, NULL, ?4, ?5, ?6, 'discord', ?7, ?8, ?9, ?10, ?11)",
        params![
            feedback.id.0,
            feedback.artifact_id.0,
            feedback.org.0,
            feedback.body,
            revision,
            feedback.parent_id.as_ref().map(|id| id.0.as_str()),
            external_author_id,
            external_author_display,
            feedback.external_created_at,
            feedback.external_edited_at,
            feedback.external_deleted_at
        ],
    )
    .map_err(internal)?;
    tx.execute(
        "INSERT INTO discord_inbound_message_state \
         (provider, external_message_id, org, artifact_id, feedback_id, \
          external_thread_id, external_author_id, external_author_display, provider_version, \
          external_created_at, external_edited_at, external_deleted_at) \
         VALUES ('discord', ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        params![
            feedback.external_message_id,
            feedback.org.0,
            feedback.artifact_id.0,
            feedback.id.0,
            thread_id,
            external_author_id,
            external_author_display,
            feedback.provider_version,
            feedback.external_created_at,
            feedback.external_edited_at,
            feedback.external_deleted_at
        ],
    )
    .map_err(internal)?;
    Ok(())
}

fn mark_policy_degraded(
    tx: &Transaction<'_>,
    binding: &TwoWayThreadAuthorization,
    _reason: ThreadDegradedReason,
) -> Result<(), AppError> {
    tx.execute(
        "UPDATE artifact_discord_inbound_policies \
            SET health='degraded', safe_error='thread_unavailable', updated_at=datetime('now') \
          WHERE artifact_id=?1 AND org=?2",
        params![binding.artifact_id.0, binding.org.0],
    )
    .map_err(internal)?;
    tx.execute(
        "UPDATE artifact_discussions \
            SET state='failed', last_error='thread_unavailable', updated_at=datetime('now') \
          WHERE artifact_id=?1 AND org=?2 AND provider='discord' \
            AND mode='discord_mirror' AND state='connected'",
        params![binding.artifact_id.0, binding.org.0],
    )
    .map(|_| ())
    .map_err(internal)
}

fn record_event(
    tx: &Transaction<'_>,
    event: &InboundEvent,
    result: InboundResult,
) -> Result<(), AppError> {
    let (result_name, safe_error) = result_projection(result);
    let (event_type, version, message_id) = event_projection(event);
    tx.execute(
        "INSERT INTO discord_inbound_events \
         (provider, event_id, org, gateway_session_id, guild_id, thread_id, message_id, \
          event_type, provider_version, payload_sha256, processed_at, result, safe_error) \
         VALUES ('discord', ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, \
                 CASE WHEN ?10='needs_fetch' THEN NULL ELSE datetime('now') END, ?10, ?11) \
         ON CONFLICT(provider, org, gateway_session_id, event_id) DO UPDATE SET \
           guild_id=excluded.guild_id, thread_id=excluded.thread_id, \
           message_id=excluded.message_id, event_type=excluded.event_type, \
           provider_version=excluded.provider_version, payload_sha256=excluded.payload_sha256, \
           processed_at=excluded.processed_at, result=excluded.result, \
           safe_error=excluded.safe_error, next_attempt_at=NULL \
         WHERE discord_inbound_events.result='needs_fetch'",
        params![
            event.event_id,
            event.org.0,
            event.gateway_session_id,
            event.guild_id,
            event.thread_id,
            message_id,
            event_type,
            version,
            event.payload_fingerprint,
            result_name,
            safe_error
        ],
    )
    .map(|_| ())
    .map_err(internal)
}

fn event_projection(event: &InboundEvent) -> (&'static str, Option<i64>, Option<&str>) {
    let kind = match event.kind {
        InboundEventKind::MessageCreate => "message_create",
        InboundEventKind::MessageUpdate => "message_update",
        InboundEventKind::MessageDelete => "message_delete",
        InboundEventKind::ThreadUpdate { .. } => "thread_update",
        InboundEventKind::ThreadDelete => "thread_delete",
    };
    (
        kind,
        event.message.as_ref().map(|message| message.version),
        event.message.as_ref().map(|message| message.id.as_str()),
    )
}

fn result_projection(result: InboundResult) -> (&'static str, &'static str) {
    match result {
        InboundResult::Applied => ("applied", ""),
        InboundResult::Duplicate => ("duplicate", ""),
        InboundResult::NeedsFetch => ("needs_fetch", "gateway_unavailable"),
        InboundResult::Degraded(_) => ("degraded", "thread_unavailable"),
        InboundResult::Ignored(reason) => (
            "ignored",
            match reason {
                IgnoreReason::Disabled => "disabled",
                IgnoreReason::Unmapped => "unmapped",
                IgnoreReason::Bot => "bot",
                IgnoreReason::Webhook => "webhook",
                IgnoreReason::Unsupported => "unsupported",
                IgnoreReason::EmptyContent => "empty_content",
                IgnoreReason::Stale => "stale",
                IgnoreReason::UnknownMessage => "unknown_message",
            },
        ),
        InboundResult::Rejected(reason) => (
            "rejected",
            match reason {
                RejectReason::CrossTenant => "cross_tenant",
                RejectReason::WrongGuild => "wrong_guild",
                RejectReason::WrongThread => "wrong_thread",
                RejectReason::InvalidEvent => "invalid_event",
            },
        ),
    }
}

struct NoFetchRest;

impl DiscordRestPort for NoFetchRest {
    fn fetch_message(
        &self,
        _guild_id: &str,
        _thread_id: &str,
        _message_id: &str,
    ) -> Result<Option<DiscordMessage>, RestError> {
        Err(RestError::Unavailable)
    }
}
