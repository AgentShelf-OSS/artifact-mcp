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

use rusqlite::{Connection, OptionalExtension, params};

use crate::error::AppError;
use crate::model::{DigestBackfillReport, StorageAuditReport};
use crate::persistence::outbox;
use crate::security::audit::{
    finalize_reconciled_receipt_in_transaction, initialize_head, verify_pending_receipts,
};

use super::digest::body_digest_at_path;
use super::durable;
use super::lifecycle::{
    FaultInjector, FaultPoint, InjectedFault, copy_recursive, io_failure, path_exists, rename,
    safe_remove, sql_error,
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

fn recover_prepared_intents(
    conn: &mut Connection,
    artifact_dir: &Path,
    rows: &[BodyRow],
    audit_key: &[u8; 32],
) -> Result<(), AppError> {
    let mut statement = conn
        .prepare(
            "SELECT id, artifact_id, operation, expected_sha256, prior_sha256, staging_path \
                  FROM artifact_durability_intents",
        )
        .map_err(|error| sql_error("prepare prepared durability intents", &error))?;
    let intents = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
            ))
        })
        .map_err(|error| sql_error("query prepared durability intents", &error))?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|error| sql_error("read prepared durability intents", &error))?;
    drop(statement);
    for (intent_id, artifact_id, operation, expected, prior, staged) in intents {
        let row = rows.iter().find(|row| row.id == artifact_id);
        let final_path = row.and_then(|row| row.body_path(artifact_dir));
        if operation == "delete" {
            if row.is_none() {
                // The database delete committed. Complete the critical cleanup only through the
                // fallible durable primitives; if either trash *or history* cannot be removed,
                // retain the intent for the next startup/operator. Deleting the marker after
                // trash alone would turn a later history-sync failure into best-effort cleanup.
                let Some(safe_id) = SafeArtifactId::parse(&artifact_id) else {
                    continue;
                };
                if let Some(trash) = transient_path_for_intent(artifact_dir, &artifact_id, &staged)
                    && path_exists(&trash)
                {
                    durable::remove(&trash)?;
                }
                durable::remove(&paths::history_dir(artifact_dir, &safe_id))?;
                complete_reconciled_intent(
                    conn,
                    audit_key,
                    &intent_id,
                    "recovered",
                    "reconciliation",
                )?;
                continue;
            }
            if let (Some(final_path), Some(row)) = (final_path, row)
                && path_exists(&final_path)
                && body_digest_at_path(&final_path, row.is_bundle).as_deref()
                    == Some(prior.as_str())
            {
                complete_reconciled_intent(conn, audit_key, &intent_id, "failure", "compensated")?;
            }
            continue;
        }
        if row.is_none() && operation == "publish" {
            // No metadata was ever committed. The staging body is unreferenced scratch and is
            // handled by the generic sweep after this marker is cleared.
            complete_reconciled_intent(conn, audit_key, &intent_id, "failure", "compensated")?;
            continue;
        }
        let Some(row) = row else { continue };
        let Some(final_path) = final_path else {
            continue;
        };
        let final_digest = path_exists(&final_path)
            .then(|| body_digest_at_path(&final_path, row.is_bundle))
            .flatten();
        if final_digest.as_deref() == Some(expected.as_str()) {
            let metadata_state =
                classify_metadata_only_intent(&intent_id, &operation, &expected, &prior, row);
            match metadata_state {
                MetadataOnlyIntent::Committed
                    if !ensure_metadata_only_history(
                        artifact_dir,
                        row,
                        &final_path,
                        &expected,
                    )? =>
                {
                    // The body is current, but its immutable predecessor snapshot is not
                    // durable. Keep the marker concealed for retry/operator repair.
                    continue;
                }
                MetadataOnlyIntent::Ambiguous => continue,
                MetadataOnlyIntent::Reverted
                    if !cleanup_reverted_metadata_snapshot_temp(artifact_dir, row)? =>
                {
                    continue;
                }
                MetadataOnlyIntent::NotApplicable
                | MetadataOnlyIntent::Committed
                | MetadataOnlyIntent::Reverted => {}
            }
            let outcome = if metadata_state == MetadataOnlyIntent::Reverted {
                "failure"
            } else {
                "recovered"
            };
            complete_reconciled_intent(
                conn,
                audit_key,
                &intent_id,
                outcome,
                if outcome == "failure" {
                    "compensated"
                } else {
                    "reconciliation"
                },
            )?;
        } else if row.body_sha256 == prior && final_digest.as_deref() == Some(prior.as_str()) {
            // Metadata never advanced. The prior body is intact, so this is an abort; discard
            // only the separately-durable staged replacement, never the last intact final body.
            let staged = transient_path_for_intent(artifact_dir, &artifact_id, &staged);
            if staged.as_ref().is_some_and(|path| path_exists(path))
                && staged.as_ref().is_some_and(|path| {
                    body_digest_at_path(path, row.is_bundle).as_deref() == Some(expected.as_str())
                })
            {
                safe_remove(staged.as_deref().expect("checked staged path"));
            }
            complete_reconciled_intent(conn, audit_key, &intent_id, "failure", "compensated")?;
        }
    }
    Ok(())
}

/// Close an intent only in the same transaction as its pending receipt's terminal recovery
/// event. Legacy intents have no receipt and remain recoverable only while the authenticated
/// pending-set commitment proves no receipt was deleted. An already-finalized/ambiguous receipt
/// is an integrity error: leaving the intent concealed is safer than silently changing the ledger.
fn complete_reconciled_intent(
    conn: &mut Connection,
    audit_key: &[u8; 32],
    intent_id: &str,
    result: &str,
    classification: &str,
) -> Result<(), AppError> {
    let mut random = [0_u8; 16];
    getrandom::fill(&mut random).map_err(|_| AppError::Internal)?;
    let event_id = format!("reconcile-{}", hex::encode(random));
    let transaction = conn
        .transaction()
        .map_err(|error| sql_error("open reconciliation audit transaction", &error))?;
    let _ = finalize_reconciled_receipt_in_transaction(
        &transaction,
        audit_key,
        intent_id,
        &event_id,
        result,
        classification,
    )?;
    if result == "failure" {
        outbox::compensate_durability_in_transaction(&transaction, intent_id)?;
    } else {
        outbox::finalize_durability_success_in_transaction(
            &transaction,
            intent_id,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |elapsed| {
                    i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX)
                }),
        )?;
    }
    transaction
        .commit()
        .map_err(|error| sql_error("commit reconciliation audit transaction", &error))
}

/// A metadata-only update has identical old/new body digests but still creates a revision and an
/// immutable history copy. Rebuild that copy from the unchanged current body before releasing its
/// intent after an interruption between metadata commit and history publication.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MetadataOnlyIntent {
    NotApplicable,
    Committed,
    Reverted,
    Ambiguous,
}

fn classify_metadata_only_intent(
    intent_id: &str,
    operation: &str,
    expected: &str,
    prior: &str,
    row: &BodyRow,
) -> MetadataOnlyIntent {
    if operation != "update" || expected != prior {
        return MetadataOnlyIntent::NotApplicable;
    }
    if row.body_sha256 != prior {
        return MetadataOnlyIntent::Ambiguous;
    }
    let Some(target) = intent_target_revision(intent_id, &row.id) else {
        return MetadataOnlyIntent::Ambiguous;
    };
    if target == row.revision {
        MetadataOnlyIntent::Committed
    } else if row.revision.checked_add(1) == Some(target) {
        MetadataOnlyIntent::Reverted
    } else {
        MetadataOnlyIntent::Ambiguous
    }
}

fn intent_target_revision(intent_id: &str, artifact_id: &str) -> Option<u64> {
    let raw = intent_id
        .strip_prefix("update:")?
        .strip_prefix(artifact_id)?
        .strip_prefix(':')?;
    if raw.starts_with('0') || !raw.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let revision = raw.parse().ok()?;
    (revision <= 9_007_199_254_740_991).then_some(revision)
}

fn ensure_metadata_only_history(
    artifact_dir: &Path,
    row: &BodyRow,
    final_path: &Path,
    digest: &str,
) -> Result<bool, AppError> {
    if row.revision < 2 {
        return Ok(false);
    }
    let Some(id) = row.safe_id.as_ref() else {
        return Ok(false);
    };
    let Some(outgoing_is_bundle) = row.outgoing_is_bundle else {
        return Ok(false);
    };
    let destination = paths::history_body_path(
        artifact_dir,
        id,
        row.revision.saturating_sub(1),
        outgoing_is_bundle,
    );
    let snapshot_temporary = paths::history_snapshot_temp_path(
        artifact_dir,
        id,
        row.revision.saturating_sub(1),
        outgoing_is_bundle,
    );
    durable::ensure_removed(&snapshot_temporary)?;
    if path_exists(&destination) {
        return Ok(
            body_digest_at_path(&destination, outgoing_is_bundle).as_deref() == Some(digest),
        );
    }
    let Some(parent) = destination.parent() else {
        return Ok(false);
    };
    durable::create_dir_all(artifact_dir, parent)?;
    let temporary = destination.with_extension("metadata-recovery-tmp");
    if path_exists(&temporary) {
        durable::remove(&temporary)?;
    }
    copy_recursive(final_path, &temporary)
        .map_err(|error| io_failure("copy metadata recovery history", &temporary, &error))?;
    durable::sync_tree(&temporary)?;
    durable::rename(&temporary, &destination)?;
    Ok(body_digest_at_path(&destination, outgoing_is_bundle).as_deref() == Some(digest))
}

fn cleanup_reverted_metadata_snapshot_temp(
    artifact_dir: &Path,
    row: &BodyRow,
) -> Result<bool, AppError> {
    let Some(id) = row.safe_id.as_ref() else {
        return Ok(false);
    };
    let temporary =
        paths::history_snapshot_temp_path(artifact_dir, id, row.revision, row.is_bundle);
    durable::ensure_removed(&temporary)?;
    Ok(!path_exists(&temporary))
}

/// Intent storage contains only an owned transient basename.  Treat any legacy absolute or
/// malformed value as untrusted evidence, never as a path the server may inspect or remove.
fn transient_path_for_intent(
    artifact_dir: &Path,
    artifact_id: &str,
    name: &str,
) -> Option<PathBuf> {
    (Path::new(name).components().count() == 1
        && paths::is_transient_name(name)
        && paths::transient_name_artifact_id(name)
            .is_some_and(|owner| owner.as_str() == artifact_id))
    .then(|| artifact_dir.join(name))
}

fn resolve_durability_intents(
    conn: &mut Connection,
    artifact_dir: &Path,
    rows: &[BodyRow],
    audit_key: &[u8; 32],
) -> Result<(), AppError> {
    let mut statement = conn
        .prepare(
            "SELECT id, artifact_id, operation, expected_sha256, prior_sha256, staging_path FROM artifact_durability_intents",
        )
        .map_err(|error| sql_error("prepare durability intent recovery", &error))?;
    let intents = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
            ))
        })
        .map_err(|error| sql_error("query durability intent recovery", &error))?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|error| sql_error("read durability intent recovery", &error))?;
    drop(statement);
    for (intent_id, artifact_id, operation, expected, prior, staged) in intents {
        let complete = if operation == "delete" {
            // Retain a delete intent while its owned trash or history evidence still exists.
            // Acknowledged deletion is only complete after *both* cleanup barriers, not merely
            // the SQL cascade or the first removal.
            let no_row = !rows.iter().any(|row| row.id == artifact_id);
            no_row
                && !intent_transient_exists(conn, artifact_dir, &intent_id)?
                && SafeArtifactId::parse(&artifact_id)
                    .is_some_and(|id| !path_exists(&paths::history_dir(artifact_dir, &id)))
                || rows
                    .iter()
                    .find(|row| row.id == artifact_id)
                    .is_some_and(|row| {
                        row.body_path(artifact_dir).is_some_and(|path| {
                            body_digest_at_path(&path, row.is_bundle).as_deref()
                                == Some(prior.as_str())
                        }) && !transient_path_for_intent(artifact_dir, &artifact_id, &staged)
                            .is_some_and(|path| path_exists(&path))
                    })
        } else if operation == "publish" && !rows.iter().any(|row| row.id == artifact_id) {
            // A body-durable publish whose metadata transaction never committed is an orphan,
            // not a public artifact. Clear the marker; the generic sweep applies ordinary
            // transient/orphan policy rather than retaining it as forensic evidence.
            true
        } else if let Some(row) = rows.iter().find(|row| row.id == artifact_id) {
            match row.body_path(artifact_dir) {
                Some(path)
                    if body_digest_at_path(&path, row.is_bundle).as_deref()
                        == Some(expected.as_str()) =>
                {
                    match classify_metadata_only_intent(
                        &intent_id, &operation, &expected, &prior, row,
                    ) {
                        MetadataOnlyIntent::Committed => {
                            ensure_metadata_only_history(artifact_dir, row, &path, &expected)?
                        }
                        MetadataOnlyIntent::Reverted => {
                            cleanup_reverted_metadata_snapshot_temp(artifact_dir, row)?
                        }
                        MetadataOnlyIntent::Ambiguous => false,
                        MetadataOnlyIntent::NotApplicable => true,
                    }
                }
                _ => false,
            }
        } else {
            false
        };
        if complete {
            let outcome = if operation == "delete" && rows.iter().any(|row| row.id == artifact_id) {
                "failure"
            } else if let Some(row) = rows.iter().find(|row| row.id == artifact_id) {
                match classify_metadata_only_intent(&intent_id, &operation, &expected, &prior, row)
                {
                    MetadataOnlyIntent::Reverted => "failure",
                    _ if row.body_sha256 == prior && expected != prior => "failure",
                    _ => "recovered",
                }
            } else {
                "recovered"
            };
            complete_reconciled_intent(
                conn,
                audit_key,
                &intent_id,
                outcome,
                if outcome == "failure" {
                    "compensated"
                } else {
                    "reconciliation"
                },
            )?;
        }
    }
    Ok(())
}

fn intent_transient_exists(
    conn: &Connection,
    artifact_dir: &Path,
    intent_id: &str,
) -> Result<bool, AppError> {
    let row = conn
        .query_row(
            "SELECT artifact_id, staging_path FROM artifact_durability_intents WHERE id = ?1",
            params![intent_id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()
        .map_err(|error| sql_error("read durability transient", &error))?;
    Ok(row.is_some_and(|(artifact_id, name)| {
        transient_path_for_intent(artifact_dir, &artifact_id, &name)
            .is_some_and(|path| path_exists(&path))
    }))
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
