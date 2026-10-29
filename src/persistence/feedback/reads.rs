use rusqlite::{Connection, OptionalExtension, params, params_from_iter};

use crate::error::AppError;
use crate::model::{
    ArtifactId, ClientId, EmailAddress, Feedback, FeedbackAuthor, FeedbackId, FeedbackRef, OrgId,
    Timestamp,
};

use super::sql::{COLUMNS, JOINED_COLUMNS};

// ---------------------------------------------------------------------------
// Reads
// ---------------------------------------------------------------------------

/// `getFeedback(id)` — [lib/feedback.js:116-118]
///
/// # Errors
/// [`AppError::Unavailable`] if the query fails.
pub fn get(conn: &Connection, id: &FeedbackId) -> Result<Option<Feedback>, AppError> {
    conn.query_row(
        &format!("SELECT {COLUMNS} FROM feedback WHERE id = ?1"),
        params![id.0],
        read_row,
    )
    .optional()
    .map_err(|error| failed("read feedback", &error))
}

/// The minimal lookup a route may perform *before* artifact authorization (contract §"Frozen
/// domain and access types"): id, artifact, org and nothing else — no body, viewer email, or
/// anchor.
///
/// # Errors
/// [`AppError::Unavailable`] if the query fails.
pub fn feedback_ref(conn: &Connection, id: &FeedbackId) -> Result<Option<FeedbackRef>, AppError> {
    conn.query_row(
        "SELECT id, artifact_id, org FROM feedback WHERE id = ?1",
        params![id.0],
        |row| {
            Ok(FeedbackRef {
                id: FeedbackId(row.get(0)?),
                artifact_id: ArtifactId(row.get(1)?),
                org: OrgId(row.get(2)?),
            })
        },
    )
    .optional()
    .map_err(|error| failed("read feedback reference", &error))
}

/// `listForArtifact(artifactId)` — [lib/feedback.js:50-52,112-114].
///
/// Frozen order: unresolved first, then oldest-first, then id — the thread reading order the
/// viewer shell renders.
///
/// # Errors
/// [`AppError::Unavailable`] if the query fails.
pub fn list_for_artifact(
    conn: &Connection,
    artifact_id: &ArtifactId,
) -> Result<Vec<Feedback>, AppError> {
    query(
        conn,
        &format!(
            "SELECT {COLUMNS} FROM feedback WHERE artifact_id = ?1 \
             ORDER BY (resolved_at IS NOT NULL), created_at ASC, id ASC"
        ),
        &[&artifact_id.0],
    )
}

/// `listAll(artifactId)` — [lib/feedback.js:153-155]. With an artifact it is
/// [`list_for_artifact`]; without one it is the admin firehose, newest-first.
///
/// # Errors
/// [`AppError::Unavailable`] if the query fails.
pub fn list_all(
    conn: &Connection,
    artifact_id: Option<&ArtifactId>,
) -> Result<Vec<Feedback>, AppError> {
    match artifact_id {
        Some(artifact_id) => list_for_artifact(conn, artifact_id),
        None => query(
            conn,
            &format!(
                "SELECT {COLUMNS} FROM feedback \
                 ORDER BY (resolved_at IS NOT NULL), created_at DESC, id DESC"
            ),
            &[],
        ),
    }
}

/// Organization-wide agent listing for reader and collaborator keys.
pub fn list_for_org(conn: &Connection, org: &OrgId) -> Result<Vec<Feedback>, AppError> {
    query(
        conn,
        &format!(
            "SELECT {COLUMNS} FROM feedback WHERE org = ?1 \
             ORDER BY (resolved_at IS NOT NULL), created_at DESC, id DESC"
        ),
        &[&org.0],
    )
}

/// `listForClient(clientId, artifactId, org)` — [lib/feedback.js:61-79,147-151].
///
/// The `org` filter is not optional cosmetics: `client_id` survives an org move
/// [lib/store.js:478], so a non-admin key that listed by `client_id` alone would keep reading the
/// *new* tenant's feedback bodies, anchors, and verified viewer emails [lib/feedback.js:69-71].
/// `None` is the admin path, which intentionally sees every org.
///
/// # Errors
/// [`AppError::Unavailable`] if the query fails.
pub fn list_for_client(
    conn: &Connection,
    client_id: &ClientId,
    artifact_id: Option<&ArtifactId>,
    org: Option<&OrgId>,
) -> Result<Vec<Feedback>, AppError> {
    let mut sql = format!(
        "SELECT {JOINED_COLUMNS} FROM feedback f \
         JOIN artifacts a ON a.id = f.artifact_id AND a.org = f.org \
         WHERE a.client_id = ?1"
    );
    let mut binds: Vec<&String> = vec![&client_id.0];
    // Node's four prepared statements differ only in these two predicates and their bind order.
    if let Some(artifact_id) = artifact_id {
        sql.push_str(" AND f.artifact_id = ?2");
        binds.push(&artifact_id.0);
    }
    if let Some(org) = org {
        sql.push_str(if artifact_id.is_some() {
            " AND a.org = ?3"
        } else {
            " AND a.org = ?2"
        });
        binds.push(&org.0);
    }
    sql.push_str(" ORDER BY (f.resolved_at IS NOT NULL), f.created_at DESC, f.id DESC");
    query(conn, &sql, &binds)
}

fn query(conn: &Connection, sql: &str, binds: &[&String]) -> Result<Vec<Feedback>, AppError> {
    let mut stmt = conn
        .prepare(sql)
        .map_err(|error| failed("prepare feedback listing", &error))?;
    let rows = stmt
        .query_map(params_from_iter(binds.iter()), read_row)
        .map_err(|error| failed("list feedback", &error))?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|error| failed("read feedback rows", &error))
}

/// Maps one row of [`COLUMNS`] / [`JOINED_COLUMNS`].
fn read_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Feedback> {
    let viewer_email = row.get::<_, Option<String>>(3)?.map(EmailAddress);
    let author = match row.get::<_, String>(17)?.as_str() {
        "discord" => FeedbackAuthor::Discord {
            external_author_id: row.get(18)?,
            external_author_display: row.get(19)?,
        },
        _ => FeedbackAuthor::Artifact {
            viewer_email: viewer_email.clone().ok_or_else(|| {
                rusqlite::Error::InvalidColumnType(
                    3,
                    "viewer_email".to_owned(),
                    rusqlite::types::Type::Null,
                )
            })?,
        },
    };
    Ok(Feedback {
        id: FeedbackId(row.get(0)?),
        artifact_id: ArtifactId(row.get(1)?),
        org: OrgId(row.get(2)?),
        viewer_email,
        author,
        body: row.get(4)?,
        artifact_revision: row.get::<_, i64>(5)?.unsigned_abs(),
        parent_id: row.get::<_, Option<String>>(6)?.map(FeedbackId),
        anchor_path: row.get(7)?,
        anchor_x: row.get(8)?,
        anchor_y: row.get(9)?,
        anchor_w: row.get(10)?,
        anchor_h: row.get(11)?,
        anchor_approx: row.get::<_, i64>(12)? != 0,
        anchor_page: row.get(13)?,
        created_at: Timestamp(row.get(14)?),
        resolved_at: row.get::<_, Option<String>>(15)?.map(Timestamp),
        resolved_by: row.get(16)?,
        external_created_at: row.get::<_, Option<String>>(20)?.map(Timestamp),
        external_edited_at: row.get::<_, Option<String>>(21)?.map(Timestamp),
        external_deleted_at: row.get::<_, Option<String>>(22)?.map(Timestamp),
        anchor_kind: row.get(23)?,
        anchor_node_id: row.get(24)?,
        anchor_quote: row.get(25)?,
        anchor_version: if row.get::<_, Option<String>>(23)?.is_some()
            || row.get::<_, Option<String>>(24)?.is_some()
            || row.get::<_, Option<String>>(25)?.is_some()
        {
            2
        } else if row.get::<_, Option<f64>>(8)?.is_some() && row.get::<_, Option<f64>>(9)?.is_some()
        {
            1
        } else {
            0
        },
    })
}

/// SQL faults are operator-facing and logged; the message never carries row data.
pub(super) fn failed(operation: &str, error: &rusqlite::Error) -> AppError {
    tracing::error!(operation, error = %error, "feedback persistence failed");
    AppError::Unavailable("database unavailable".to_owned())
}
