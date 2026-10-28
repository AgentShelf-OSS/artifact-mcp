//! SQL row mapping, metadata lookups, durability intents, and revision rows.

use super::*;

// ---------------------------------------------------------------------------
// SQL
// ---------------------------------------------------------------------------

/// Every v21 `artifacts` column, in the order [`meta_from_row`] reads them.
pub(super) const META_COLUMNS: &str = "id, client_id, org, title, description, bytes, created_at, updated_at, \
     uploader_label, owner_email, is_bundle, entry, revision, category, hidden, body_sha256, \
     artifact_key";

/// Conflict message for an `(org, artifact_key)` unique-index violation during publish
/// (ADR-0015). The MCP layer re-resolves the key once when it sees exactly this conflict.
pub const ARTIFACT_KEY_TAKEN: &str = "artifact_key is already in use";

/// Columns of `artifact_revisions`, in the order [`revision_from_row`] reads them.
pub(super) const REVISION_COLUMNS: &str = "artifact_id, org, revision, title, description, category, bytes, \
     is_bundle, entry, body_sha256, created_at, client_id";

fn u64_column(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<u64> {
    let value: i64 = row.get(index)?;
    u64::try_from(value).map_err(|_| rusqlite::Error::IntegralValueOutOfRange(index, value))
}

fn meta_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ArtifactMeta> {
    Ok(ArtifactMeta {
        id: ArtifactId(row.get(0)?),
        client_id: ClientId(row.get(1)?),
        org: OrgId(row.get(2)?),
        title: row.get(3)?,
        description: row.get(4)?,
        bytes: u64_column(row, 5)?,
        created_at: Timestamp(row.get(6)?),
        updated_at: Timestamp(row.get(7)?),
        uploader_label: row.get(8)?,
        owner_email: row.get(9)?,
        is_bundle: row.get::<_, i64>(10)? != 0,
        entry: row.get(11)?,
        revision: u64_column(row, 12)?,
        category: row.get(13)?,
        hidden: row.get::<_, i64>(14)? != 0,
        body_sha256: row.get(15)?,
        artifact_key: row.get(16)?,
    })
}

pub(super) fn revision_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ArtifactRevision> {
    Ok(ArtifactRevision {
        artifact_id: ArtifactId(row.get(0)?),
        org: OrgId(row.get(1)?),
        revision: u64_column(row, 2)?,
        title: row.get(3)?,
        description: row.get(4)?,
        category: row.get(5)?,
        bytes: u64_column(row, 6)?,
        is_bundle: row.get::<_, i64>(7)? != 0,
        entry: row.get(8)?,
        body_sha256: row.get(9)?,
        created_at: Timestamp(row.get(10)?),
        client_id: row.get::<_, Option<String>>(11)?.map(ClientId),
    })
}

pub(crate) fn sql_error(operation: &'static str, error: &rusqlite::Error) -> AppError {
    tracing::error!(operation, error = %error, "artifact lifecycle sql failed");
    AppError::Internal
}

pub(super) fn as_i64(value: u64) -> Result<i64, AppError> {
    i64::try_from(value).map_err(|_| AppError::Internal)
}

/// `getMetaStmt.get(id)` — [lib/store.js:111]
pub(super) fn load_meta(conn: &Connection, id: &str) -> Result<Option<ArtifactMeta>, AppError> {
    if durability_pending(conn, id)? {
        return Ok(None);
    }
    load_meta_unchecked(conn, id)
}

/// `getArtifactMetaByKey(org, key)` — ready rows only, exactly like [`load_meta`].
pub(super) fn load_meta_by_key(
    conn: &Connection,
    org: &str,
    artifact_key: &str,
) -> Result<Option<ArtifactMeta>, AppError> {
    let id: Option<String> = conn
        .query_row(
            "SELECT id FROM artifacts WHERE org = ?1 AND artifact_key = ?2",
            params![org, artifact_key],
            |row| row.get(0),
        )
        .optional()
        .map_err(|error| sql_error("load artifact by key", &error))?;
    match id {
        Some(id) => load_meta(conn, &id),
        None => Ok(None),
    }
}

pub(super) fn is_artifact_key_conflict(error: &rusqlite::Error) -> bool {
    matches!(
        error,
        rusqlite::Error::SqliteFailure(failure, Some(message))
            if failure.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE
                && message.contains("artifact_key")
    )
}

pub(super) fn load_meta_unchecked(
    conn: &Connection,
    id: &str,
) -> Result<Option<ArtifactMeta>, AppError> {
    conn.query_row(
        &format!("SELECT {META_COLUMNS} FROM artifacts WHERE id = ?1"),
        params![id],
        meta_from_row,
    )
    .optional()
    .map_err(|error| sql_error("load artifact metadata", &error))
}

/// A row with a prepared durability intent is deliberately invisible to normal read paths.  This
/// prevents a concurrent request from observing metadata while its body is being staged, moved,
/// or removed. Startup reconciliation clears only intents whose postcondition is verified.
pub(super) fn start_durability_intent(
    conn: &Connection,
    intent_id: &str,
    artifact_id: &str,
    operation: &str,
    expected_sha256: &str,
    prior_sha256: &str,
    staging_path: &Path,
) -> Result<(), AppError> {
    // SQLite is not an authority for filesystem paths. Persist only the generated transient
    // basename after proving it belongs to this artifact; reconciliation reconstructs it beneath
    // the configured artifact directory.
    let staging_name = staging_path
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| {
            paths::is_transient_name(name)
                && paths::transient_name_artifact_id(name)
                    .is_some_and(|owner| owner.as_str() == artifact_id)
        })
        .ok_or(AppError::Internal)?;
    conn.execute(
        "INSERT INTO artifact_durability_intents \
         (id, artifact_id, operation, state, expected_sha256, prior_sha256, staging_path) \
         VALUES (?1, ?2, ?3, 'prepared', ?4, ?5, ?6)",
        params![
            intent_id,
            artifact_id,
            operation,
            expected_sha256,
            prior_sha256,
            staging_name
        ],
    )
    .map_err(|error| sql_error("record durability intent", &error))?;
    Ok(())
}

pub(super) fn advance_durability_intent(
    conn: &Connection,
    intent_id: &str,
    state: &str,
) -> Result<(), AppError> {
    let changes = conn.execute(
        "UPDATE artifact_durability_intents SET state = ?1, updated_at = datetime('now') WHERE id = ?2",
        params![state, intent_id],
    )
    .map_err(|error| sql_error("advance durability intent", &error))?;
    if changes != 1 {
        return Err(AppError::Internal);
    }
    Ok(())
}

pub(super) fn finish_durability_intent(conn: &Connection, intent_id: &str) -> Result<(), AppError> {
    conn.execute(
        "DELETE FROM artifact_durability_intents WHERE id = ?1",
        params![intent_id],
    )
    .map_err(|error| sql_error("complete durability intent", &error))?;
    Ok(())
}

pub(super) fn list_metas(
    conn: &Connection,
    sql: &str,
    parameters: &[&dyn rusqlite::ToSql],
) -> Result<Vec<ArtifactMeta>, AppError> {
    let mut statement = conn
        .prepare(sql)
        .map_err(|error| sql_error("prepare artifact list", &error))?;
    let rows = statement
        .query_map(parameters, meta_from_row)
        .map_err(|error| sql_error("query artifact list", &error))?;
    let rows = rows
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|error| sql_error("read artifact list", &error))?;
    rows.into_iter()
        .filter_map(|meta| match durability_pending(conn, &meta.id.0) {
            Ok(false) => Some(Ok(meta)),
            Ok(true) => None,
            Err(error) => Some(Err(error)),
        })
        .collect()
}

fn durability_pending(conn: &Connection, id: &str) -> Result<bool, AppError> {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM artifact_durability_intents WHERE artifact_id = ?1)",
        params![id],
        |row| row.get::<_, i64>(0),
    )
    .map(|value| value != 0)
    .map_err(|error| sql_error("check durability intent", &error))
}

pub(super) fn durability_intent_state(
    conn: &Connection,
    intent_id: &str,
) -> Result<Option<String>, AppError> {
    conn.query_row(
        "SELECT state FROM artifact_durability_intents WHERE id = ?1",
        [intent_id],
        |row| row.get(0),
    )
    .optional()
    .map_err(|error| sql_error("read durability intent state", &error))
}

/// Persist attribution and metadata for one produced revision.
pub(super) fn record_revision_row(
    conn: &Connection,
    meta: &ArtifactMeta,
    acting_client_id: Option<&ClientId>,
) -> Result<(), AppError> {
    conn
        .execute(
            "INSERT OR REPLACE INTO artifact_revisions \
             (artifact_id, org, revision, title, description, category, bytes, is_bundle, entry, body_sha256, client_id) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                meta.id.0,
                meta.org.0,
                as_i64(meta.revision)?,
                meta.title,
                meta.description,
                meta.category,
                as_i64(meta.bytes)?,
                i64::from(meta.is_bundle),
                meta.entry,
                meta.body_sha256,
                acting_client_id.map(|client_id| &client_id.0),
            ],
        )
        .map_err(|error| sql_error("record revision row", &error))?;
    Ok(())
}

/// Preserve a pre-v22 live revision without falsely assigning it to the next editing key.
pub(super) fn record_legacy_revision_row(
    conn: &Connection,
    meta: &ArtifactMeta,
) -> Result<(), AppError> {
    conn.execute(
        "INSERT OR IGNORE INTO artifact_revisions \
         (artifact_id, org, revision, title, description, category, bytes, is_bundle, entry, body_sha256, client_id) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, NULL)",
        params![
            meta.id.0,
            meta.org.0,
            as_i64(meta.revision)?,
            meta.title,
            meta.description,
            meta.category,
            as_i64(meta.bytes)?,
            i64::from(meta.is_bundle),
            meta.entry,
            meta.body_sha256,
        ],
    )
    .map_err(|error| sql_error("record legacy revision row", &error))?;
    Ok(())
}

/// `getRevisionStmt.get(id, revision)` — [lib/store.js:163-165]
pub(super) fn load_revision(
    conn: &Connection,
    id: &str,
    revision: u64,
) -> Result<Option<ArtifactRevision>, AppError> {
    conn.query_row(
        &format!(
            "SELECT {REVISION_COLUMNS} FROM artifact_revisions \
             WHERE artifact_id = ?1 AND revision = ?2 \
               AND revision < (SELECT revision FROM artifacts WHERE id = ?1)"
        ),
        params![id, as_i64(revision)?],
        revision_from_row,
    )
    .optional()
    .map_err(|error| sql_error("load revision row", &error))
}

/// `orgExists(name)` = `SELECT 1 FROM orgs WHERE name = ?` — [lib/orgs.js:17], [lib/orgs.js:53-55]
///
/// U09 owns `persistence/orgs.rs` and has not landed yet; this is the single read-only statement
/// `store.js` needs, and it moves to U09's adapter when that unit lands.
pub(super) fn org_exists(conn: &Connection, name: &str) -> Result<bool, AppError> {
    conn.query_row("SELECT 1 FROM orgs WHERE name = ?1", params![name], |_| {
        Ok(())
    })
    .optional()
    .map(|found: Option<()>| found.is_some())
    .map_err(|error| sql_error("check organization", &error))
}
