use std::path::{Path, PathBuf};

use rusqlite::{Connection, OptionalExtension, params};

use crate::artifacts::digest::body_digest_at_path;
use crate::artifacts::durable;
use crate::artifacts::lifecycle::{
    copy_recursive, io_failure, path_exists, safe_remove, sql_error,
};
use crate::artifacts::paths::{self, SafeArtifactId};
use crate::error::AppError;
use crate::persistence::outbox;
use crate::security::audit::finalize_reconciled_receipt_in_transaction;

use super::support::BodyRow;

pub(super) fn recover_prepared_intents(
    conn: &mut Connection,
    artifact_dir: &Path,
    rows: &[BodyRow],
    audit_key: &[u8; 32],
) -> Result<(), AppError> {
    let intents = load_prepared_intents(conn)?;
    for intent in intents {
        recover_prepared_intent(conn, artifact_dir, rows, audit_key, intent)?;
    }
    Ok(())
}

type PreparedIntent = (String, String, String, String, String, String);

fn load_prepared_intents(conn: &Connection) -> Result<Vec<PreparedIntent>, AppError> {
    let mut statement = conn
        .prepare(
            "SELECT id, artifact_id, operation, expected_sha256, prior_sha256, staging_path \\
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
    Ok(intents)
}

fn recover_prepared_intent(
    conn: &mut Connection,
    artifact_dir: &Path,
    rows: &[BodyRow],
    audit_key: &[u8; 32],
    (intent_id, artifact_id, operation, expected, prior, staged): PreparedIntent,
) -> Result<(), AppError> {
    let row = rows.iter().find(|row| row.id == artifact_id);
    let final_path = row.and_then(|row| row.body_path(artifact_dir));
    if operation == "delete" {
        if row.is_none() {
            // The database delete committed. Complete the critical cleanup only through the
            // fallible durable primitives; if either trash *or history* cannot be removed,
            // retain the intent for the next startup/operator. Deleting the marker after
            // trash alone would turn a later history-sync failure into best-effort cleanup.
            let Some(safe_id) = SafeArtifactId::parse(&artifact_id) else {
                return Ok(());
            };
            if let Some(trash) = transient_path_for_intent(artifact_dir, &artifact_id, &staged)
                && path_exists(&trash)
            {
                durable::remove(&trash)?;
            }
            durable::remove(&paths::history_dir(artifact_dir, &safe_id))?;
            complete_reconciled_intent(conn, audit_key, &intent_id, "recovered", "reconciliation")?;
            return Ok(());
        }
        if let (Some(final_path), Some(row)) = (final_path, row)
            && path_exists(&final_path)
            && body_digest_at_path(&final_path, row.is_bundle).as_deref() == Some(prior.as_str())
        {
            complete_reconciled_intent(conn, audit_key, &intent_id, "failure", "compensated")?;
        }
        return Ok(());
    }
    if row.is_none() && operation == "publish" {
        // No metadata was ever committed. The staging body is unreferenced scratch and is
        // handled by the generic sweep after this marker is cleared.
        complete_reconciled_intent(conn, audit_key, &intent_id, "failure", "compensated")?;
        return Ok(());
    }
    let Some(row) = row else { return Ok(()) };
    let Some(final_path) = final_path else {
        return Ok(());
    };
    let final_digest = path_exists(&final_path)
        .then(|| body_digest_at_path(&final_path, row.is_bundle))
        .flatten();
    if final_digest.as_deref() == Some(expected.as_str()) {
        let metadata_state =
            classify_metadata_only_intent(&intent_id, &operation, &expected, &prior, row);
        match metadata_state {
            MetadataOnlyIntent::Committed
                if !ensure_metadata_only_history(artifact_dir, row, &final_path, &expected)? =>
            {
                // The body is current, but its immutable predecessor snapshot is not
                // durable. Keep the marker concealed for retry/operator repair.
                return Ok(());
            }
            MetadataOnlyIntent::Ambiguous => return Ok(()),
            MetadataOnlyIntent::Reverted
                if !cleanup_reverted_metadata_snapshot_temp(artifact_dir, row)? =>
            {
                return Ok(());
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
pub(super) fn transient_path_for_intent(
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

pub(super) fn resolve_durability_intents(
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
