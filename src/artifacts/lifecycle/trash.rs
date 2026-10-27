//! Restore a past revision, and delete an artifact into the trash.

use super::*;

impl StoreContext {
    // -----------------------------------------------------------------------
    // restore
    // -----------------------------------------------------------------------

    /// `restoreById(id, revision, …)` — replay a past revision as a NEW revision.
    /// [lib/store.js:605-624]
    ///
    /// `update` snapshots the *current* revision first, so a restore is itself undoable.
    pub(super) fn restore_sync(
        &self,
        conn: &mut Connection,
        id: &str,
        revision: u64,
        acting_client_id: Option<ClientId>,
        audit: MutationAudit,
    ) -> Result<RestoreArtifactResult, AppError> {
        let _gate = self
            .lifecycle_gate
            .write()
            .map_err(|_| AppError::Internal)?;
        let Some(meta) = load_meta(conn, id)? else {
            return Err(AppError::NotFound("not_found".to_owned()));
        };
        let safe_id = SafeArtifactId::parse(id).ok_or(AppError::ConcealedNotFound)?;
        let Some(row) = load_revision(conn, id, revision)? else {
            return Err(AppError::NotFound("revision_not_found".to_owned()));
        };
        if row.is_bundle != meta.is_bundle {
            return Err(AppError::Conflict("type_mismatch".to_owned()));
        }
        let body_path =
            paths::history_body_path(&self.artifact_dir, &safe_id, row.revision, row.is_bundle);
        if !path_exists(&body_path) {
            return Err(AppError::Gone("body_missing".to_owned()));
        }

        let content = if row.is_bundle {
            // `readTree` returns the snapshot and the entry is supplied explicitly, so the map's
            // ordering never reaches entry auto-selection. [lib/store.js:617-618]
            let files = read::read_tree(&body_path)?.into_iter().collect();
            ArtifactContent::Bundle {
                files,
                entry: Some(row.entry.clone()),
            }
        } else {
            let html = std::fs::read(&body_path)
                .map_err(|error| io_failure("read history body", &body_path, &error))?;
            ArtifactContent::SingleHtml(String::from_utf8_lossy(&html).into_owned())
        };

        let update = ArtifactUpdate {
            expected_revision: meta.revision,
            acting_client_id,
            title: Some(row.title.clone()),
            description: Some(row.description.clone()),
            category: Some(row.category.clone()),
            content: Some(content),
        };
        let result =
            self.update_locked(conn, id, meta.revision, &update, &audit, "artifact.restore")?;
        Ok(RestoreArtifactResult {
            meta: result.meta,
            restored_from: row.revision,
        })
    }

    // -----------------------------------------------------------------------
    // delete
    // -----------------------------------------------------------------------

    /// `removeById(id, …)` — [lib/store.js:626-644]
    pub(super) fn delete_sync(
        &self,
        conn: &mut Connection,
        id: &str,
        audit: MutationAudit,
    ) -> Result<bool, AppError> {
        let _gate = self
            .lifecycle_gate
            .write()
            .map_err(|_| AppError::Internal)?;
        let Some(meta) = load_meta(conn, id)? else {
            return Ok(false);
        };
        let audit = audit.for_affected_tenant(&meta.org);
        let safe_id = SafeArtifactId::parse(id).ok_or(AppError::ConcealedNotFound)?;
        let trash = self.transient_path(&safe_id, TransientKind::Trash)?;
        let intent_id = format!("delete:{id}:{}", meta.revision);
        // Reserve the immutable audit snapshot before the first filesystem side effect.  This
        // transaction is the linearization point for a delete intent: failures roll both rows
        // back, while a later ambiguous crash leaves both for reconciliation.
        let transaction = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| sql_error("open delete audit reservation", &error))?;
        start_durability_intent(
            &transaction,
            &intent_id,
            id,
            "delete",
            "",
            &meta.body_sha256,
            &trash,
        )?;
        self.reserve_audit(
            &transaction,
            &audit,
            &intent_id,
            "artifact.delete",
            id,
            Some(meta.revision),
        )?;
        transaction
            .commit()
            .map_err(|error| sql_error("commit delete audit reservation", &error))?;

        // `moveBodyToTrash` runs OUTSIDE the try: a failure here has nothing to compensate.
        // [lib/store.js:503-509], [lib/store.js:630]
        let move_handle = MovedBody {
            source: self.body_path(&safe_id, meta.is_bundle),
            trash: trash.clone(),
        };
        let moved = match self.move_body_to_trash(&safe_id, &meta, trash) {
            Ok(moved) => moved,
            Err(error) => {
                // A directory fsync can fail after the kernel completed rename.  Do not clear
                // the marker in that physical-partial state: normal reads stay concealed and
                // startup reconciliation can prove whether trash must be restored/cleaned.
                if path_exists(&move_handle.trash) && !path_exists(&move_handle.source) {
                    return Err(error);
                }
                self.cancel_compensated_audit_intent(
                    conn,
                    &audit,
                    &intent_id,
                    "artifact.delete",
                    id,
                    Some(meta.revision),
                )?;
                return Err(error);
            }
        };

        match self.delete_attempt(conn, &safe_id, &meta, moved.as_ref(), &intent_id) {
            Ok(deleted) => {
                self.finalize_audit_and_intent(
                    conn,
                    &audit,
                    &intent_id,
                    "artifact.delete",
                    id,
                    Some(meta.revision),
                )?;
                Ok(deleted)
            }
            Err(interrupt) => {
                if interrupt.compensate {
                    self.restore_body(moved.as_ref())?;
                    self.cancel_compensated_audit_intent(
                        conn,
                        &audit,
                        &intent_id,
                        "artifact.delete",
                        id,
                        Some(meta.revision),
                    )?;
                }
                Err(interrupt.error)
            }
        }
    }

    fn delete_attempt(
        &self,
        conn: &mut Connection,
        id: &SafeArtifactId,
        pre_delete_meta: &ArtifactMeta,
        moved: Option<&MovedBody>,
        intent_id: &str,
    ) -> Result<bool, Interrupt> {
        self.fault(FaultPoint::DeleteRow)?;
        // Reactions cascade on `artifacts(id)`; feedback, revisions, views, and shares cascade on
        // the composite `artifacts(id, org)`. [lib/migrations.js:124,167,261,299,357]
        let transaction = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| {
                Interrupt::from(sql_error("open delete metadata transaction", &error))
            })?;
        let changes = transaction
            .execute("DELETE FROM artifacts WHERE id = ?1", params![id.as_str()])
            .map_err(|error| Interrupt::from(sql_error("delete artifact", &error)))?;
        if changes == 0 {
            // The row vanished after the audited reservation (for example a competing trusted
            // maintenance action).  This is a proven compensated failure, never a successful
            // delete: restore the body and let the caller release the receipt atomically.
            drop(transaction);
            self.restore_body(moved)?;
            return Err(Interrupt {
                error: AppError::Conflict("conflict".to_owned()),
                compensate: true,
            });
        }

        self.fanout_delivery(
            &transaction,
            pre_delete_meta,
            WebhookEvent::Deleted,
            intent_id,
        )
        .map_err(Interrupt::from)?;

        let changes = transaction
            .execute(
                "UPDATE artifact_durability_intents SET state = 'metadata_committed', updated_at = datetime('now') WHERE id = ?1",
                params![intent_id],
            )
            .map_err(|error| Interrupt::from(sql_error("advance delete durability intent", &error)))?;
        if changes != 1 {
            return Err(Interrupt::from(AppError::Internal));
        }
        transaction.commit().map_err(|error| {
            Interrupt::from(sql_error("commit delete metadata transaction", &error))
        })?;

        if let Err(fault) = self.faults.check(FaultPoint::DeleteTrashRemove) {
            return Err(Interrupt {
                error: fault.into_error(),
                compensate: false,
            });
        }
        if let Some(moved) = moved {
            durable::remove(&moved.trash).map_err(|error| Interrupt {
                error,
                compensate: false,
            })?;
        }

        if let Err(fault) = self.faults.check(FaultPoint::DeleteHistoryRemove) {
            return Err(Interrupt {
                error: fault.into_error(),
                compensate: false,
            });
        }
        // Revision rows cascade through the composite FK; this removes their bodies.
        // [lib/store.js:638]
        durable::remove(&paths::history_dir(&self.artifact_dir, id)).map_err(|error| {
            Interrupt {
                error,
                compensate: false,
            }
        })?;
        advance_durability_intent(conn, intent_id, "body_durable").map_err(Interrupt::from)?;
        Ok(true)
    }

    /// `moveBodyToTrash(id, meta)` — [lib/store.js:503-509]
    fn move_body_to_trash(
        &self,
        id: &SafeArtifactId,
        meta: &ArtifactMeta,
        trash: PathBuf,
    ) -> Result<Option<MovedBody>, AppError> {
        let source = self.body_path(id, meta.is_bundle);
        if !path_exists(&source) {
            return Ok(None);
        }
        self.bare_fault(FaultPoint::DeleteTrashRename)?;
        self.rename_with_barrier(&source, &trash, FaultPoint::DeleteTrashRenameBarrier)?;
        Ok(Some(MovedBody { source, trash }))
    }

    /// `restoreBody(moved)` — [lib/store.js:511-514]
    fn restore_body(&self, moved: Option<&MovedBody>) -> Result<(), AppError> {
        self.bare_fault(FaultPoint::DeleteRestoreBody)?;
        let Some(moved) = moved else {
            return Ok(());
        };
        if !path_exists(&moved.trash) {
            return Ok(());
        }
        rename(&moved.trash, &moved.source)
    }
}
