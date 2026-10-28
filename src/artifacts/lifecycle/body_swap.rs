//! Update body swap: snapshot the live body, rename the staged body in, revert, and prune
//! retained history.

use super::*;

impl StoreContext {
    /// `snapshotBody` then the staged rename. [lib/store.js:420-428]
    pub(super) fn swap_body(
        &self,
        id: &SafeArtifactId,
        meta: &ArtifactMeta,
        staged: Option<&Path>,
        snapshot: &mut Option<BodySnapshot>,
    ) -> Result<(), Interrupt> {
        self.fault(FaultPoint::UpdateSnapshot)?;
        match self.snapshot_body(id, meta, staged.is_some()) {
            Ok(value) => *snapshot = value,
            Err(error) => {
                let source = self.body_path(id, meta.is_bundle);
                let history =
                    paths::history_body_path(&self.artifact_dir, id, meta.revision, meta.is_bundle);
                // The move completed but its directory barrier failed. Metadata already names the
                // staged replacement; retaining the intent lets recovery install it while the
                // immutable outgoing body remains in history. Never compensate into a missing
                // live body here.
                if staged.is_some() && !path_exists(&source) && path_exists(&history) {
                    return Err(Interrupt {
                        error,
                        compensate: false,
                    });
                }
                // A metadata-only snapshot copies the live body. If the copy's final rename
                // completed but its directory barrier failed, the source remains live and the
                // digest-valid immutable history is already the recoverable half of the new
                // revision. Keep its intent/metadata concealed for reconciliation instead of
                // reverting and stranding an immutable destination that blocks every retry.
                if staged.is_none()
                    && path_exists(&source)
                    && body_digest_at_path(&history, meta.is_bundle).as_deref()
                        == Some(meta.body_sha256.as_str())
                {
                    return Err(Interrupt {
                        error,
                        compensate: false,
                    });
                }
                return Err(Interrupt::from(error));
            }
        }

        self.fault(FaultPoint::UpdateSwap)?;
        if let Some(path) = staged {
            let final_path = self.body_path(id, meta.is_bundle);
            if let Err(error) =
                self.rename_with_barrier(path, &final_path, FaultPoint::UpdateSwapBarrier)
            {
                if path_exists(&final_path) && !path_exists(path) {
                    return Err(Interrupt {
                        error,
                        compensate: false,
                    });
                }
                return Err(Interrupt::from(error));
            }
        }
        Ok(())
    }

    /// `snapshotBody(id, meta, { moveBody })` — [lib/store.js:549-561]
    ///
    /// `moveBody` relocates the live body (freeing the final path for the replacement);
    /// otherwise it is copied, because a metadata-only update keeps its body live.
    fn snapshot_body(
        &self,
        id: &SafeArtifactId,
        meta: &ArtifactMeta,
        move_body: bool,
    ) -> Result<Option<BodySnapshot>, AppError> {
        let source = self.body_path(id, meta.is_bundle);
        if !path_exists(&source) {
            return Err(AppError::Gone("body_missing".to_owned()));
        }
        let destination =
            paths::history_body_path(&self.artifact_dir, id, meta.revision, meta.is_bundle);
        if let Some(parent) = destination.parent() {
            durable::create_dir_all_with_barrier(&self.artifact_dir, parent, || {
                self.bare_fault(FaultPoint::UpdateHistoryDirectorySync)
            })?;
        }
        // A history destination is immutable for a revision. Replacing it would first destroy
        // the only known good snapshot if a previous crash left an unexpected entry there.
        if path_exists(&destination) {
            return Err(AppError::Conflict(
                "history destination already exists".to_owned(),
            ));
        }
        if move_body {
            self.rename_with_barrier(&source, &destination, FaultPoint::UpdateSnapshotBarrier)?;
            return Ok(Some(BodySnapshot {
                source,
                destination,
                moved: true,
            }));
        }
        let temporary = paths::history_snapshot_temp_path(
            &self.artifact_dir,
            id,
            meta.revision,
            meta.is_bundle,
        );
        if path_exists(&temporary) {
            return Err(AppError::Conflict(
                "history snapshot temporary already exists".to_owned(),
            ));
        }
        copy_recursive(&source, &temporary)
            .map_err(|error| io_failure("copy history snapshot", &temporary, &error))?;
        durable::sync_tree_with_barrier(&temporary, || {
            self.bare_fault(FaultPoint::UpdateSnapshotFileSync)
        })?;
        self.rename_with_barrier(&temporary, &destination, FaultPoint::UpdateSnapshotBarrier)?;
        Ok(Some(BodySnapshot {
            source,
            destination,
            moved: false,
        }))
    }

    pub(super) fn remove_snapshot_temporary(
        &self,
        id: &SafeArtifactId,
        revision: u64,
        is_bundle: bool,
    ) -> Result<(), AppError> {
        let temporary =
            paths::history_snapshot_temp_path(&self.artifact_dir, id, revision, is_bundle);
        if path_exists(&temporary) {
            self.bare_fault(FaultPoint::UpdateSnapshotTempRemove)?;
            durable::remove_with_barrier(&temporary, || {
                self.bare_fault(FaultPoint::UpdateSnapshotTempRemoveBarrier)
            })?;
        }
        Ok(())
    }

    /// Prove the outgoing body still matches its committed metadata before admitting a revision.
    pub(super) fn require_current_body(
        &self,
        id: &SafeArtifactId,
        meta: &ArtifactMeta,
    ) -> Result<(), AppError> {
        let path = self.body_path(id, meta.is_bundle);
        let Some(digest) = body_digest_at_path(&path, meta.is_bundle) else {
            return Err(AppError::Gone("body_missing".to_owned()));
        };
        if digest != meta.body_sha256 {
            return Err(AppError::Conflict("body_digest_mismatch".to_owned()));
        }
        Ok(())
    }

    /// `restoreSnapshotBody(snap)` — only a *moved* snapshot is undone. [lib/store.js:562-565]
    pub(super) fn restore_snapshot_body(
        &self,
        snapshot: Option<&BodySnapshot>,
    ) -> Result<(), AppError> {
        self.bare_fault(FaultPoint::UpdateRestoreSnapshot)?;
        let Some(snapshot) = snapshot else {
            return Ok(());
        };
        if !snapshot.moved || !path_exists(&snapshot.destination) {
            return Ok(());
        }
        rename(&snapshot.destination, &snapshot.source)
    }

    /// `deleteRevisionStmt.run(id, meta.revision); restoreMetaStmt.run(before);` in one
    /// transaction. [lib/store.js:432-435]
    pub(super) fn revert_update(
        &self,
        conn: &mut Connection,
        meta: &ArtifactMeta,
        before: &MetaBefore,
    ) -> Result<(), AppError> {
        self.bare_fault(FaultPoint::UpdateRevertMetadata)?;
        let transaction = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| sql_error("open revert transaction", &error))?;
        transaction
            .execute(
                "DELETE FROM artifact_revisions WHERE artifact_id = ?1 AND revision = ?2",
                params![meta.id.0, as_i64(before.revision.saturating_add(1))?],
            )
            .map_err(|error| sql_error("delete revision row", &error))?;
        transaction
            .execute(
                "UPDATE artifacts \
                 SET title = ?1, description = ?2, bytes = ?3, entry = ?4, category = ?5, \
                     body_sha256 = ?6, revision = ?7, updated_at = ?8 \
                 WHERE id = ?9",
                params![
                    before.title,
                    before.description,
                    as_i64(before.bytes)?,
                    before.entry,
                    before.category,
                    before.body_sha256,
                    as_i64(before.revision)?,
                    before.updated_at,
                    meta.id.0,
                ],
            )
            .map_err(|error| sql_error("revert artifact metadata", &error))?;
        transaction
            .commit()
            .map_err(|error| sql_error("commit revert transaction", &error))
    }

    /// `pruneHistory(id)` — best effort, exactly like Node's swallowing `try`.
    /// [lib/store.js:568-575]
    pub(super) fn prune_history(&self, conn: &Connection, id: &SafeArtifactId) {
        let Ok(offset) = i64::try_from(self.limits.max_history) else {
            return;
        };
        let Ok(mut statement) = conn.prepare(
            "SELECT revision, is_bundle FROM artifact_revisions \
             WHERE artifact_id = ?1 \
               AND revision < (SELECT revision FROM artifacts WHERE id = ?1) \
             ORDER BY revision DESC LIMIT -1 OFFSET ?2",
        ) else {
            return;
        };
        let prunable = statement
            .query_map(params![id.as_str(), offset], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)? != 0))
            })
            .and_then(std::iter::Iterator::collect::<rusqlite::Result<Vec<_>>>);
        let Ok(prunable) = prunable else {
            return;
        };
        for (revision, is_bundle) in prunable {
            let _ = conn.execute(
                "DELETE FROM artifact_revisions WHERE artifact_id = ?1 AND revision = ?2",
                params![id.as_str(), revision],
            );
            let Ok(revision) = u64::try_from(revision) else {
                continue;
            };
            safe_remove(&paths::history_body_path(
                &self.artifact_dir,
                id,
                revision,
                is_bundle,
            ));
        }
    }
}
