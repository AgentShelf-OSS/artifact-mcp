//! Artifact discussion rows and the authority a mirror needs before it can be enabled.

use rusqlite::{Connection, OptionalExtension, params};

use super::{
    connection_rows::connection_row,
    internal,
    types::{ArtifactDiscussion, DiscussionConnectionStrategy, DiscussionMode, DiscussionState},
};
use crate::{
    error::AppError,
    integrations::delivery_envelope::stable_delivery_event_id,
    model::{ArtifactId, OrgId, Timestamp, WebhookEvent},
};

pub(super) const DISCUSSION_COLUMNS: &str = "artifact_id, org, provider, mode, connection_org, connection_id, thread_id, \
    root_message_id, state, generation, enabled_by, enabled_at, disabled_at, last_synced_at, \
    last_error, created_at, updated_at, anchor_outbox_id";

pub(super) struct MirrorAuthority {
    pub(super) connection_id: String,
    pub(super) anchor_outbox_id: Option<String>,
}

pub(super) fn mirror_authority(
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

pub(super) fn discussion_row(
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

pub(super) fn artifact_exists(
    conn: &Connection,
    artifact_id: &str,
    org: &str,
) -> Result<bool, AppError> {
    conn.query_row(
        "SELECT 1 FROM artifacts WHERE id = ?1 AND org = ?2",
        params![artifact_id, org],
        |_| Ok(()),
    )
    .optional()
    .map(|row: Option<()>| row.is_some())
    .map_err(internal)
}
