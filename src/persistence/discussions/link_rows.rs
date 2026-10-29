//! Feedback-to-Discord message link rows and their lookups.

use rusqlite::{Connection, OptionalExtension, Row, params};

use super::{internal, types::DiscussionMessageLink};
use crate::{
    error::AppError,
    model::{ArtifactId, FeedbackId, OrgId, Timestamp},
};

pub(super) const LINK_COLUMNS: &str = "provider, artifact_id, org, connection_id, feedback_id, delivery_event_id, outbox_id, \
    tombstone_outbox_id, external_thread_id, external_message_id, source, generation, state, last_error, local_deleted_at, \
    created_at, updated_at, posted_at";

pub(super) fn link_by_feedback(
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

pub(super) fn link_by_outbox(
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

pub(super) fn link_by_feedback_generation(
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

pub(super) fn link_by_tombstone_outbox(
    conn: &Connection,
    outbox_id: &str,
) -> Result<Option<DiscussionMessageLink>, AppError> {
    conn.query_row(
        &format!("SELECT {LINK_COLUMNS} FROM discussion_message_links WHERE provider = 'discord' AND tombstone_outbox_id = ?1"),
        [outbox_id],
        discussion_link_row,
    ).optional().map_err(internal)
}

pub(super) fn discussion_link_row(row: &Row<'_>) -> rusqlite::Result<DiscussionMessageLink> {
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
