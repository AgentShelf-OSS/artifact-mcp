use std::path::Path;

use rusqlite::{Connection, params};

use crate::artifacts::digest::body_digest_at_path;
use crate::artifacts::lifecycle::{FaultInjector, FaultPoint, path_exists, sql_error};
use crate::error::AppError;
use crate::model::DigestBackfillReport;

use super::support::{body_row_from, into_error};

/// `backfillBodyDigests()` — [lib/store.js:755-770]
///
/// Repairs rows created before the v17 `artifact-body-digest` migration, which added the column
/// with an empty default and never hashed the existing bodies. It is metadata repair, not a
/// content mutation: **the revision and `updated_at` are deliberately untouched**, and the guarded
/// `WHERE` clause makes it idempotent — a second run selects nothing.
///
/// # Errors
/// Returns [`AppError::Internal`] for a database failure, or the injected fault when
/// [`FaultPoint::BackfillWrite`] is armed.
pub fn backfill_body_digests(
    conn: &Connection,
    artifact_dir: &Path,
    faults: &dyn FaultInjector,
) -> Result<DigestBackfillReport, AppError> {
    let mut statement = conn
        .prepare(
            "SELECT id, is_bundle FROM artifacts WHERE body_sha256 IS NULL OR body_sha256 = ''",
        )
        .map_err(|error| sql_error("prepare digest backfill query", &error))?;
    let rows = statement
        .query_map([], |row| body_row_from(row, false))
        .map_err(|error| sql_error("query digest backfill rows", &error))?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|error| sql_error("read digest backfill rows", &error))?;

    let mut report = DigestBackfillReport {
        scanned: rows.len(),
        updated: 0,
    };
    for row in rows {
        let Some(final_path) = row.body_path(artifact_dir) else {
            continue;
        };
        if !path_exists(&final_path) {
            continue;
        }
        let Some(digest) = body_digest_at_path(&final_path, row.is_bundle) else {
            continue;
        };
        faults
            .check(FaultPoint::BackfillWrite)
            .map_err(into_error)?;
        conn.execute(
            "UPDATE artifacts SET body_sha256 = ?1 \
             WHERE id = ?2 AND (body_sha256 IS NULL OR body_sha256 = '')",
            params![digest, row.id],
        )
        .map_err(|error| sql_error("backfill body digest", &error))?;
        // Node counts an attempted repair, not `info.changes`. [lib/store.js:766-767]
        report.updated += 1;
    }
    Ok(report)
}
