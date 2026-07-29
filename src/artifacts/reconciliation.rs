//! Owned by U08 (sol) — storage reconciliation and digest backfill.
//!
//! A process or host crash can interrupt the cross-resource sequences in [`super::lifecycle`]
//! between the SQLite commit and the filesystem move that follows it. ADR-0002 answers that with
//! startup reconciliation: recover what is provably recoverable, discard only what is provably
//! transient, and **report — never delete** genuine divergence.
//!
//! Node oracle: `auditStorage({ cleanTransient })` — [lib/store.js:675-744] — and
//! `backfillBodyDigests()` — [lib/store.js:755-770]. `server.js:89` runs the audit with
//! `cleanTransient: true` as startup step 8, then the backfill at `server.js:102`.
//!
//! # The crash pre-states, and what happens to each
//!
//! | Pre-state | Decision | Node |
//! |---|---|---|
//! | `.‹id›.staging-…` present, final path **empty** | rename staging into place (`recovered_paths`) | [lib/store.js:701-704] |
//! | `.‹id›.staging-…` present, final path occupied, staged digest = committed `body_sha256` | preserve the outgoing body in history, then install the staged committed content | [lib/store.js:705-719] |
//! | `.‹id›.staging-…` present, final path occupied, staged digest missing/mismatched | preserve both paths and report the transient plus installed divergence | [lib/store.js:705-710] |
//! | `.‹id›.trash-…` present, final path empty | rename it back into place (an interrupted delete whose row survived) | [lib/store.js:701-704] |
//! | any transient path with no live row, or whose final body already matches | remove the transient path | [lib/store.js:713-715] |
//! | live row with **no** body | reported in `missing_bodies`; nothing is deleted | [lib/store.js:734-736] |
//! | body with **no** row | reported in `orphan_bodies`; nothing is deleted | [lib/store.js:717-719] |
//! | `.history/‹id›` with no row | removed when cleaning (its revision rows already cascaded) | [lib/store.js:722-733] |
//!
//! `.staging-` entries are processed **before** `.trash-` entries ([lib/store.js:688-691]) so that
//! when both survive one crash the committed *new* body wins and the old body is discarded, never
//! the other way around.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, params};

use crate::error::AppError;
use crate::model::{DigestBackfillReport, StorageAuditReport};

use super::digest::body_digest_at_path;
use super::lifecycle::{
    FaultInjector, FaultPoint, InjectedFault, io_failure, path_exists, rename, safe_remove,
    sql_error,
};
use super::paths::{self, SafeArtifactId, TransientKind};

/// One row of the current-body reconciliation query — [lib/store.js:120]
#[derive(Debug, Clone)]
struct BodyRow {
    id: String,
    safe_id: Option<SafeArtifactId>,
    is_bundle: bool,
    body_sha256: String,
    revision: u64,
    outgoing_is_bundle: Option<bool>,
}

impl BodyRow {
    /// `row.is_bundle ? bundleDir(row.id) : filePath(row.id)` — [lib/store.js:700]
    ///
    /// `None` when the stored id cannot be a path segment. Every generated id satisfies
    /// `/^[0-9a-z]{6,24}$/` ([lib/store.js:30]), so this only guards a hand-edited database.
    fn body_path(&self, artifact_dir: &Path) -> Option<PathBuf> {
        self.safe_id
            .as_ref()
            .map(|id| paths::body_path(artifact_dir, id, self.is_bundle))
    }

    /// ``rows.map((row) => row.is_bundle ? row.id : `${row.id}.html`)`` — [lib/store.js:678]
    fn expected_name(&self) -> String {
        if self.is_bundle {
            self.id.clone()
        } else {
            format!("{}.{}", self.id, paths::SINGLE_BODY_EXTENSION)
        }
    }
}

fn body_row_from(row: &rusqlite::Row<'_>, with_digest: bool) -> rusqlite::Result<BodyRow> {
    let id: String = row.get(0)?;
    let safe_id = SafeArtifactId::parse(&id);
    Ok(BodyRow {
        id,
        safe_id,
        is_bundle: row.get::<_, i64>(1)? != 0,
        body_sha256: if with_digest {
            row.get::<_, Option<String>>(2)?.unwrap_or_default()
        } else {
            String::new()
        },
        revision: if with_digest {
            row.get::<_, i64>(3)?.unsigned_abs()
        } else {
            0
        },
        outgoing_is_bundle: if with_digest {
            row.get::<_, Option<i64>>(4)?.map(|value| value != 0)
        } else {
            None
        },
    })
}

fn load_body_rows(conn: &Connection) -> Result<Vec<BodyRow>, AppError> {
    let mut statement = conn
        .prepare(
            "SELECT a.id, a.is_bundle, a.body_sha256, a.revision, \
                    (SELECT r.is_bundle FROM artifact_revisions r \
                     WHERE r.artifact_id = a.id AND r.revision = a.revision - 1) \
             FROM artifacts a",
        )
        .map_err(|error| sql_error("prepare storage audit query", &error))?;
    statement
        .query_map([], |row| body_row_from(row, true))
        .map_err(|error| sql_error("query storage audit rows", &error))?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|error| sql_error("read storage audit rows", &error))
}

/// Directory entries in Node's processing order: `.staging-` first, then `.trash-`, then
/// everything else. [lib/store.js:688-691]
///
/// Node keeps `readdirSync` order inside a rank because `Array.prototype.sort` is stable; this
/// sorts by name inside a rank instead, which is deterministic across runtimes and filesystems.
/// The two can only differ when a single artifact has two transient paths of the same kind, an
/// order the reference does not define either.
fn ranked_entries(artifact_dir: &Path) -> Result<Vec<String>, AppError> {
    let mut names: Vec<String> = Vec::new();
    let entries = match std::fs::read_dir(artifact_dir) {
        Ok(entries) => entries,
        // Node would throw; at startup a missing directory just means an empty store, and both
        // `lib/db.js` and `Database::open_at` create it.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(names),
        Err(error) => return Err(io_failure("list artifact directory", artifact_dir, &error)),
    };
    for entry in entries {
        let entry = entry
            .map_err(|error| io_failure("read artifact directory entry", artifact_dir, &error))?;
        names.push(entry.file_name().to_string_lossy().into_owned());
    }
    names.sort_by(|left, right| {
        transient_rank(left)
            .cmp(&transient_rank(right))
            .then_with(|| left.cmp(right))
    });
    Ok(names)
}

/// `const rank = (n) => (n.includes(".staging-") ? 0 : n.includes(".trash-") ? 1 : 2);`
/// — [lib/store.js:689]
fn transient_rank(name: &str) -> u8 {
    if name.contains(&staging_marker()) {
        0
    } else if name.contains(&trash_marker()) {
        1
    } else {
        2
    }
}

/// `".staging-"` — [lib/store.js:689], [lib/store.js:705]
fn staging_marker() -> String {
    format!(".{}-", TransientKind::Staging.as_str())
}

/// `".trash-"` — [lib/store.js:689]
fn trash_marker() -> String {
    format!(".{}-", TransientKind::Trash.as_str())
}

/// `auditStorage({ cleanTransient })` — [lib/store.js:675-744]
///
/// With `clean_transient = false` nothing is written: transient paths are listed and divergence is
/// reported. With `true` (startup) the recoverable states above are repaired.
///
/// # Errors
/// Returns [`AppError::Internal`] for a database or filesystem failure, or the injected fault when
/// a [`FaultPoint`] is armed.
pub fn audit_storage(
    conn: &Connection,
    artifact_dir: &Path,
    clean_transient: bool,
    faults: &dyn FaultInjector,
) -> Result<StorageAuditReport, AppError> {
    let rows = load_body_rows(conn)?;
    let expected: BTreeSet<String> = rows.iter().map(BodyRow::expected_name).collect();
    let mut report = StorageAuditReport::default();

    for name in ranked_entries(artifact_dir)? {
        // The version-history store is not an orphan body. [lib/store.js:693]
        if name == paths::HISTORY_DIR_NAME {
            continue;
        }
        if paths::is_transient_name(&name) {
            report.transient_paths.push(name.clone());
            if clean_transient {
                reconcile_transient(artifact_dir, &name, &rows, &mut report, faults)?;
            }
        } else if !expected.contains(&name) {
            report.orphan_bodies.push(name);
        }
    }

    if clean_transient {
        reclaim_orphan_history(artifact_dir, &rows, &mut report, faults)?;
    }

    // Both lists are computed AFTER recovery, so a body that was just reinstated is not reported
    // as missing. [lib/store.js:734-742]
    for row in &rows {
        match row.body_path(artifact_dir) {
            Some(path) if path_exists(&path) => {
                if !row.body_sha256.is_empty()
                    && body_digest_at_path(&path, row.is_bundle).as_ref() != Some(&row.body_sha256)
                {
                    report.divergent_bodies.push(row.id.clone());
                }
            }
            _ => report.missing_bodies.push(row.id.clone()),
        }
    }

    Ok(report)
}

/// The `cleanTransient` branch for one transient directory entry. [lib/store.js:695-716]
fn reconcile_transient(
    artifact_dir: &Path,
    name: &str,
    rows: &[BodyRow],
    report: &mut StorageAuditReport,
    faults: &dyn FaultInjector,
) -> Result<(), AppError> {
    let transient = artifact_dir.join(name);
    let owner = paths::transient_name_artifact_id(name)
        .and_then(|id| rows.iter().find(|row| row.id == id.as_str()));

    // No live record owns this path, or its id cannot address one: unreferenced scratch.
    // [lib/store.js:713-715]
    let Some(final_path) = owner.and_then(|row| row.body_path(artifact_dir)) else {
        faults
            .check(FaultPoint::ReconcileDiscard)
            .map_err(into_error)?;
        safe_remove(&transient);
        return Ok(());
    };
    let Some(row) = owner else {
        return Ok(());
    };

    if !path_exists(&final_path) {
        // The interrupted body belongs at the (now-empty) final path. This covers an interrupted
        // publish, an interrupted update swap, and an interrupted delete whose row survived.
        // [lib/store.js:701-704]
        faults
            .check(FaultPoint::ReconcileRecover)
            .map_err(into_error)?;
        rename(&transient, &final_path)?;
        report.recovered_paths.push(name.to_owned());
        return Ok(());
    }

    let staged = name.contains(&staging_marker());
    let installed_matches = row.body_sha256.is_empty()
        || body_digest_at_path(&final_path, row.is_bundle).as_ref() == Some(&row.body_sha256);
    if staged && !installed_matches {
        let staged_matches = !row.body_sha256.is_empty()
            && body_digest_at_path(&transient, row.is_bundle).as_ref() == Some(&row.body_sha256);
        if !staged_matches {
            // The staged body is not the content named by committed metadata. Preserve both
            // paths; transient_paths already names staging, and the post-recovery scan reports
            // the installed body as divergent without destroying either copy.
            // [lib/store.js:705-710]
            return Ok(());
        }
        // A staged body survived AND the installed body does not match the committed metadata
        // digest: the process crashed after committing the new revision but before swapping the
        // body in. Preserve the outgoing revision before installing the committed replacement,
        // exactly as the uninterrupted update path does. [lib/store.js:705-714]
        faults
            .check(FaultPoint::ReconcileRecover)
            .map_err(into_error)?;
        preserve_outgoing_body(artifact_dir, row, &final_path)?;
        rename(&transient, &final_path)?;
        report.recovered_paths.push(name.to_owned());
        return Ok(());
    }

    faults
        .check(FaultPoint::ReconcileDiscard)
        .map_err(into_error)?;
    safe_remove(&transient);
    Ok(())
}

/// Move the installed outgoing revision to its history path before recovery installs staging.
/// The metadata transaction records that outgoing revision atomically with the current revision;
/// if no such row exists, Node falls back to replacing the installed path without a snapshot.
fn preserve_outgoing_body(
    artifact_dir: &Path,
    row: &BodyRow,
    final_path: &Path,
) -> Result<(), AppError> {
    let Some(id) = row.safe_id.as_ref() else {
        safe_remove(final_path);
        return Ok(());
    };
    let Some(outgoing_is_bundle) = row.outgoing_is_bundle else {
        safe_remove(final_path);
        return Ok(());
    };
    let outgoing_revision = row.revision.saturating_sub(1);
    let source = paths::body_path(artifact_dir, id, outgoing_is_bundle);
    let destination =
        paths::history_body_path(artifact_dir, id, outgoing_revision, outgoing_is_bundle);
    if let Some(parent) = destination.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| io_failure("create recovery history directory", parent, &error))?;
    }
    safe_remove(&destination);
    rename(&source, &destination)
}

/// Reclaim history for artifacts that no longer exist (e.g. a crash between the row delete and
/// `removeHistory`). The revision rows already cascade-deleted; this removes their bodies.
/// [lib/store.js:722-733]
fn reclaim_orphan_history(
    artifact_dir: &Path,
    rows: &[BodyRow],
    report: &mut StorageAuditReport,
    faults: &dyn FaultInjector,
) -> Result<(), AppError> {
    let history_root = paths::history_root(artifact_dir);
    if !path_exists(&history_root) {
        return Ok(());
    }
    let live: BTreeSet<&str> = rows.iter().map(|row| row.id.as_str()).collect();
    let mut names: Vec<String> = Vec::new();
    let entries = std::fs::read_dir(&history_root)
        .map_err(|error| io_failure("list history directory", &history_root, &error))?;
    for entry in entries {
        let entry = entry
            .map_err(|error| io_failure("read history directory entry", &history_root, &error))?;
        names.push(entry.file_name().to_string_lossy().into_owned());
    }
    names.sort();
    for name in names {
        if !live.contains(name.as_str()) {
            faults
                .check(FaultPoint::ReconcileOrphanHistory)
                .map_err(into_error)?;
            safe_remove(&history_root.join(&name));
            report.orphan_history.push(name);
        }
    }
    Ok(())
}

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

fn into_error(fault: InjectedFault) -> AppError {
    match fault {
        InjectedFault::Error(error) | InjectedFault::Crash(error) => error,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranks_staging_before_trash_before_everything_else() {
        assert_eq!(transient_rank(".abc123.staging-xyz"), 0);
        assert_eq!(transient_rank(".abc123.trash-xyz"), 1);
        assert_eq!(transient_rank("abc123.html"), 2);
        assert_eq!(transient_rank(paths::HISTORY_DIR_NAME), 2);
    }

    #[test]
    fn expected_names_follow_the_frozen_layout() {
        let single = BodyRow {
            id: "abc123def456".to_owned(),
            safe_id: SafeArtifactId::parse("abc123def456"),
            is_bundle: false,
            body_sha256: String::new(),
            revision: 1,
            outgoing_is_bundle: None,
        };
        let bundle = BodyRow {
            is_bundle: true,
            ..single.clone()
        };
        assert_eq!(single.expected_name(), "abc123def456.html");
        assert_eq!(bundle.expected_name(), "abc123def456");
        assert_eq!(
            single.body_path(Path::new("/data/artifacts")),
            Some(PathBuf::from("/data/artifacts/abc123def456.html"))
        );
        assert_eq!(
            bundle.body_path(Path::new("/data/artifacts")),
            Some(PathBuf::from("/data/artifacts/abc123def456"))
        );
    }
}
