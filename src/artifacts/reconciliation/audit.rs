use std::collections::BTreeSet;
use std::path::Path;

use rusqlite::{Connection, OptionalExtension, params};

use crate::artifacts::digest::body_digest_at_path;
use crate::artifacts::lifecycle::{
    FaultInjector, FaultPoint, io_failure, path_exists, rename, safe_remove, sql_error,
};
use crate::artifacts::paths;
use crate::error::AppError;
use crate::model::StorageAuditReport;
use crate::security::audit::{initialize_head, verify_pending_receipts};

use super::intents::{
    recover_prepared_intents, resolve_durability_intents, transient_path_for_intent,
};
use super::support::{BodyRow, into_error, load_body_rows, ranked_entries, staging_marker};

/// `auditStorage({ cleanTransient })` — [lib/store.js:675-744]
///
/// With `clean_transient = false` nothing is written: transient paths are listed and divergence is
/// reported. With `true` (startup) the recoverable states above are repaired.
///
/// # Errors
/// Returns [`AppError::Internal`] for a database or filesystem failure, or the injected fault when
/// a [`FaultPoint`] is armed.
pub fn audit_storage(
    conn: &mut Connection,
    artifact_dir: &Path,
    clean_transient: bool,
    faults: &dyn FaultInjector,
    audit_key: &[u8; 32],
) -> Result<StorageAuditReport, AppError> {
    if clean_transient {
        // A missing receipt changes the head-authenticated pending-set commitment. Check before
        // any filesystem repair or legacy no-receipt bypass can release a durability intent.
        initialize_head(conn, audit_key)?;
        verify_pending_receipts(conn, audit_key)?;
    }
    let mut rows = load_body_rows(conn)?;
    let mut report = StorageAuditReport::default();

    // Intent-aware recovery runs before the generic transient sweep.  A prepared update whose
    // database still names the prior digest is an aborted update, not disposable unknown scratch;
    // a prepared delete with a live row restores trash before any code can remove it.
    if clean_transient {
        recover_prepared_intents(conn, artifact_dir, &rows, audit_key)?;
    }
    // Intent recovery may have restored trash or completed a database-visible transition. Reload
    // before the generic sweep so it never acts on stale expectations.
    rows = load_body_rows(conn)?;
    let expected: BTreeSet<String> = rows.iter().map(BodyRow::expected_name).collect();

    for name in ranked_entries(artifact_dir)? {
        // The version-history store is not an orphan body. [lib/store.js:693]
        if name == paths::HISTORY_DIR_NAME {
            continue;
        }
        if paths::is_transient_name(&name) {
            report.transient_paths.push(name.clone());
            if clean_transient {
                reconcile_transient(conn, artifact_dir, &name, &rows, &mut report, faults)?;
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

    // Intent rows hide an artifact from concurrent normal reads while a body mutation is in
    // flight.  Only clear one after the postcondition is observable after recovery.  Ambiguous
    // rows stay visible to operators in SQLite and keep the artifact concealed; no viable copy
    // is deleted merely to make an intent disappear.
    if clean_transient {
        // The generic sweep can install a validated staging body.  Resolve against a fresh view,
        // not the pre-sweep rows, so recovery does not leave a ready body concealed.
        rows = load_body_rows(conn)?;
        resolve_durability_intents(conn, artifact_dir, &rows, audit_key)?;
    }

    Ok(report)
}
/// The `cleanTransient` branch for one transient directory entry. [lib/store.js:695-716]
fn reconcile_transient(
    conn: &Connection,
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
        if transient_is_recovery_evidence(conn, artifact_dir, name)? {
            return Ok(());
        }
        faults
            .check(FaultPoint::ReconcileDiscard)
            .map_err(into_error)?;
        safe_remove(&transient);
        return Ok(());
    };
    let Some(row) = owner else {
        return Ok(());
    };

    let transient_matches = !row.body_sha256.is_empty()
        && body_digest_at_path(&transient, row.is_bundle).as_deref()
            == Some(row.body_sha256.as_str());
    if !path_exists(&final_path) {
        // The interrupted body belongs at the (now-empty) final path. This covers an interrupted
        // publish, an interrupted update swap, and an interrupted delete whose row survived.
        // [lib/store.js:701-704]
        // A truncated/corrupt transient is evidence, not a replacement for the committed body.
        if !transient_matches {
            return Ok(());
        }
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
        // Refuse to overwrite the last viable installed body when no immutable outgoing
        // revision exists.  `preserve_outgoing_body` is deliberately non-destructive.
        if row.outgoing_is_bundle.is_none() {
            return Ok(());
        }
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

fn transient_is_recovery_evidence(
    conn: &Connection,
    artifact_dir: &Path,
    name: &str,
) -> Result<bool, AppError> {
    let owner = paths::transient_name_artifact_id(name).map(|owner| owner.as_str().to_owned());
    let Some(owner) = owner else { return Ok(false) };
    let stored: Option<String> = conn
        .query_row(
            "SELECT staging_path FROM artifact_durability_intents WHERE artifact_id = ?1",
            params![owner],
            |row| row.get(0),
        )
        .optional()
        .map_err(|error| sql_error("read transient durability evidence", &error))?;
    Ok(stored.is_some_and(|stored| {
        transient_path_for_intent(artifact_dir, &owner, &stored)
            .is_some_and(|path| path == artifact_dir.join(name))
    }))
}

/// Move the installed outgoing revision to its history path before recovery installs staging.
/// The metadata transaction records that outgoing revision atomically with the current revision;
/// if no such row exists, Node falls back to replacing the installed path without a snapshot.
fn preserve_outgoing_body(
    artifact_dir: &Path,
    row: &BodyRow,
    _final_path: &Path,
) -> Result<(), AppError> {
    let Some(id) = row.safe_id.as_ref() else {
        return Ok(());
    };
    let Some(outgoing_is_bundle) = row.outgoing_is_bundle else {
        // The committed metadata says the installed final is outgoing, but no immutable revision
        // row can own it. Preserve and report rather than deleting the last viable body.
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
    if path_exists(&destination) {
        return Ok(());
    }
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
