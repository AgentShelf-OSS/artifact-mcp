//! Update: plan, commit, and compensate an in-place artifact update.

use super::*;

impl StoreContext {
    // -----------------------------------------------------------------------
    // update
    // -----------------------------------------------------------------------

    /// `update({ … })` — [lib/store.js:314-442]
    ///
    /// `expected_revision` is already resolved by the caller: Node substitutes `meta.revision`
    /// when the client omitted it ([lib/store.js:318], [lib/mcp.js:407]), and the frozen
    /// [`ArtifactUpdate`] has no "absent" state.
    pub(super) fn update_sync(
        &self,
        conn: &mut Connection,
        id: &str,
        expected_revision: u64,
        update: &ArtifactUpdate,
        audit: &MutationAudit,
        operation: &str,
    ) -> Result<UpdateArtifactResult, AppError> {
        let _gate = self
            .lifecycle_gate
            .write()
            .map_err(|_| AppError::Internal)?;
        self.update_locked(conn, id, expected_revision, update, audit, operation)
    }

    /// Update while the caller already owns the lifecycle write gate. Restore uses this to keep
    /// the source revision's metadata/body read and its replay as one linearized operation.
    pub(super) fn update_locked(
        &self,
        conn: &mut Connection,
        id: &str,
        expected_revision: u64,
        update: &ArtifactUpdate,
        audit: &MutationAudit,
        operation: &str,
    ) -> Result<UpdateArtifactResult, AppError> {
        let Some(meta) = load_meta(conn, id)? else {
            return Err(AppError::NotFound("not_found".to_owned()));
        };
        let scoped_audit = audit.for_affected_tenant(&meta.org);
        let audit = &scoped_audit;
        let safe_id = SafeArtifactId::parse(id).ok_or(AppError::ConcealedNotFound)?;

        // `if (!Number.isInteger(g) || g < 1 || g !== meta.revision) return conflict;`
        // — [lib/store.js:319-321]
        if expected_revision < 1 || expected_revision != meta.revision {
            return Err(AppError::Conflict("conflict".to_owned()));
        }

        let plan = self.plan_update(&safe_id, &meta, update)?;
        if !plan.changed {
            // An exact no-op creates NO revision, no history snapshot, and no replacement body.
            // [lib/store.js:365-370]
            return Ok(UpdateArtifactResult {
                meta,
                changed: false,
            });
        }

        // Every new revision must retain a durable, immutable predecessor. Validate the live
        // body before metadata can advance: otherwise a metadata-only update could finish with
        // no restore point, and a content update could overwrite the last good body.
        self.require_current_body(&safe_id, &meta)?;

        // Stage the replacement before touching the database. [lib/store.js:374-384]
        let mut staged: Option<PathBuf> = None;
        let intent_id = format!("update:{id}:{}", meta.revision.saturating_add(1));
        // Even a metadata-only revision snapshots the current body into immutable history. It is
        // therefore a cross-resource mutation too: conceal it until that snapshot is durable.
        let intent_path = self.transient_path(&safe_id, TransientKind::Staging)?;
        if plan.content_changed {
            self.bare_fault(FaultPoint::UpdateStageWrite)?;
        }
        start_durability_intent(
            conn,
            &intent_id,
            id,
            "update",
            &plan.body_sha256,
            &meta.body_sha256,
            &intent_path,
        )?;
        if plan.content_changed {
            let path = intent_path;
            let staging_result = match &plan.body {
                StagedBody::Single(html) => self.stage_single_body(
                    &path,
                    html,
                    FaultPoint::UpdateStageFileSync,
                    FaultPoint::UpdateStageDirectorySync,
                ),
                StagedBody::Bundle(files) => self.stage_bundle_files(
                    &path,
                    files,
                    FaultPoint::UpdateStageFileSync,
                    FaultPoint::UpdateStageDirectorySync,
                ),
                StagedBody::None => Ok(()),
            };
            if let Err(error) = staging_result {
                safe_remove(&path);
                let _ = finish_durability_intent(conn, &intent_id);
                return Err(error);
            }
            staged = Some(path);
        }

        let before = MetaBefore {
            title: meta.title.clone(),
            description: meta.description.clone(),
            bytes: meta.bytes,
            entry: meta.entry.clone(),
            category: meta.category.clone(),
            body_sha256: meta.body_sha256.clone(),
            revision: meta.revision,
            updated_at: meta.updated_at.0.clone(),
        };

        if let Err(fault) = self.faults.check(FaultPoint::UpdateCommit) {
            let interrupt = Interrupt::from(fault);
            if interrupt.compensate {
                if let Some(path) = &staged {
                    safe_remove(path);
                }
                let _ = finish_durability_intent(conn, &intent_id);
            }
            return Err(interrupt.error);
        }

        // Commit metadata FIRST, then swap the body. [lib/store.js:386-393]
        let committed = match self.commit_update(
            conn,
            &meta,
            expected_revision,
            &plan,
            update.acting_client_id.as_ref(),
            &intent_id,
            audit,
            operation,
        ) {
            Ok(committed) => committed,
            Err(error) => {
                // Fanout/capacity errors occur before the metadata transaction commits and can
                // be proven by the still-prepared intent.  A commit error is ambiguous: retain
                // the intent, metadata, staging and blocked rows for reconciliation instead.
                if matches!(
                    &error,
                    AppError::RateLimited
                        | AppError::PayloadTooLarge
                        | AppError::Validation(_)
                        | AppError::Conflict(_)
                ) && durability_intent_state(conn, &intent_id)?.as_deref() == Some("prepared")
                {
                    if let Some(path) = &staged {
                        safe_remove(path);
                    }
                    finish_durability_intent(conn, &intent_id)?;
                }
                return Err(error);
            }
        };
        if !committed {
            // `if (!committed) { if (staged) safeRemove(staged); return conflict; }`
            // — [lib/store.js:416-419]
            if let Some(path) = &staged {
                safe_remove(path);
            }
            let _ = finish_durability_intent(conn, &intent_id);
            return Err(AppError::Conflict("conflict".to_owned()));
        }
        let mut snapshot: Option<BodySnapshot> = None;
        if let Err(interrupt) = self.swap_body(&safe_id, &meta, staged.as_deref(), &mut snapshot) {
            if interrupt.compensate {
                // Node's catch, in order: restore the snapshot, drop the staged body, revert the
                // committed metadata and its revision row. [lib/store.js:429-437]
                self.restore_snapshot_body(snapshot.as_ref())?;
                if let Some(path) = &staged {
                    safe_remove(path);
                }
                self.revert_update(conn, &meta, &before)?;
                self.remove_snapshot_temporary(&safe_id, meta.revision, meta.is_bundle)?;
                self.cancel_compensated_audit_intent(
                    conn,
                    audit,
                    &intent_id,
                    operation,
                    id,
                    Some(meta.revision.saturating_add(1)),
                )?;
            }
            return Err(interrupt.error);
        }

        advance_durability_intent(conn, &intent_id, "body_durable")?;

        // Pruning is best-effort in Node too; a fault here leaves a fully committed update.
        self.bare_fault(FaultPoint::UpdatePrune)?;
        self.prune_history(conn, &safe_id);

        self.finalize_audit_and_intent(
            conn,
            audit,
            &intent_id,
            operation,
            id,
            Some(meta.revision.saturating_add(1)),
        )?;

        let meta = load_meta(conn, id)?.ok_or(AppError::Internal)?;
        Ok(UpdateArtifactResult {
            meta,
            changed: true,
        })
    }

    /// Validation, digesting, and change detection — everything before the first side effect.
    /// [lib/store.js:323-372]
    fn plan_update(
        &self,
        id: &SafeArtifactId,
        meta: &ArtifactMeta,
        update: &ArtifactUpdate,
    ) -> Result<UpdatePlan, AppError> {
        let wants_single = matches!(update.content, Some(ArtifactContent::SingleHtml(_)));
        // Contract-delta request 1: an empty file list plus an entry is Node's "entry only"
        // update; an empty file list on its own still reaches `files is empty`.
        let entry_only = matches!(
            &update.content,
            Some(ArtifactContent::Bundle { files, entry }) if files.is_empty() && entry.is_some()
        );
        let wants_bundle =
            matches!(update.content, Some(ArtifactContent::Bundle { .. })) && !entry_only;

        if wants_single && meta.is_bundle {
            return Err(AppError::Validation(
                "artifact is a bundle; pass files, not html".to_owned(),
            ));
        }
        if wants_bundle && !meta.is_bundle {
            return Err(AppError::Validation(
                "artifact is single-file; pass html, not files".to_owned(),
            ));
        }
        if entry_only && !meta.is_bundle {
            return Err(AppError::Validation(
                "artifact is single-file; entry only applies to bundles".to_owned(),
            ));
        }

        let next_title = update
            .title
            .as_deref()
            .map_or_else(|| meta.title.clone(), |value| normalize_title(Some(value)));
        let next_description = update.description.as_deref().map_or_else(
            || meta.description.clone(),
            |value| normalize_description(Some(value)),
        );
        let next_category = update
            .category
            .as_deref()
            .map_or_else(|| meta.category.clone(), normalize_category);

        let mut next_bytes = meta.bytes;
        let mut next_entry = meta.entry.clone();
        let mut next_body_sha256 = meta.body_sha256.clone();
        let mut body = StagedBody::None;

        match &update.content {
            Some(ArtifactContent::SingleHtml(html)) => {
                let validated = validation::validate_single_body(html, &self.limits)?;
                next_bytes = validated.bytes;
                next_body_sha256 = validated.body_sha256;
                body = StagedBody::Single(html.clone());
            }
            Some(ArtifactContent::Bundle { files, entry }) if wants_bundle => {
                let built = validation::validate_bundle(
                    files,
                    entry.as_deref(),
                    Some(&meta.entry),
                    &self.limits,
                )?;
                next_bytes = built.total_bytes;
                next_entry = built.entry.clone();
                next_body_sha256 = bundle_manifest_digest(&built.files);
                body = StagedBody::Bundle(built.files);
            }
            Some(ArtifactContent::Bundle { entry, .. }) => {
                // Entry-only: the directory stays live and is snapshotted by copy.
                // [lib/store.js:354-360]
                let requested = entry.as_deref().unwrap_or_default();
                let selected = if requested.is_empty() {
                    Some(meta.entry.clone())
                } else {
                    sanitize_relative_path(requested)
                };
                let usable = selected.as_ref().is_some_and(|value| {
                    !value.is_empty()
                        && read::read_bundle_file(&self.artifact_dir, id, value, None).is_some()
                });
                let Some(selected) = selected.filter(|_| usable) else {
                    return Err(AppError::Validation(format!(
                        "entry \"{requested}\" is not one of the files"
                    )));
                };
                next_entry = selected;
            }
            None => {}
        }

        // [lib/store.js:362-364]
        let content_changed = next_bytes != meta.bytes || next_body_sha256 != meta.body_sha256;
        let changed = content_changed
            || next_entry != meta.entry
            || next_title != meta.title
            || next_description != meta.description
            || next_category != meta.category;

        Ok(UpdatePlan {
            title: next_title,
            description: next_description,
            category: next_category,
            bytes: next_bytes,
            entry: next_entry,
            body_sha256: next_body_sha256,
            body: if content_changed {
                body
            } else {
                StagedBody::None
            },
            content_changed,
            changed,
        })
    }

    /// The metadata transaction: guarded UPDATE plus the OUTGOING revision row, atomically.
    /// [lib/store.js:406-415]
    ///
    /// The guard pins `client_id` and `org` to the values read at the start of the operation, so
    /// a concurrent re-tenant or delete turns into a conflict instead of a lost update.
    #[allow(clippy::too_many_arguments)] // Transactional lifecycle inputs mirror the durability record exactly.
    fn commit_update(
        &self,
        conn: &mut Connection,
        meta: &ArtifactMeta,
        expected_revision: u64,
        plan: &UpdatePlan,
        acting_client_id: Option<&ClientId>,
        intent_id: &str,
        audit: &MutationAudit,
        operation: &str,
    ) -> Result<bool, AppError> {
        let transaction = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| sql_error("open update transaction", &error))?;
        let changes = transaction
            .execute(
                "UPDATE artifacts \
                 SET title = ?1, description = ?2, bytes = ?3, entry = ?4, category = ?5, \
                     body_sha256 = ?6, revision = revision + 1, updated_at = datetime('now') \
                 WHERE id = ?7 AND client_id = ?8 AND org = ?9 AND revision = ?10",
                params![
                    plan.title,
                    plan.description,
                    as_i64(plan.bytes)?,
                    plan.entry,
                    plan.category,
                    plan.body_sha256,
                    meta.id.0,
                    meta.client_id.0,
                    meta.org.0,
                    as_i64(expected_revision)?,
                ],
            )
            .map_err(|error| sql_error("update artifact metadata", &error))?;
        if changes != 1 {
            // `if (info.changes !== 1) return false;` — dropping the transaction rolls it back.
            return Ok(false);
        }
        self.reserve_audit(
            &transaction,
            audit,
            intent_id,
            operation,
            &meta.id.0,
            Some(meta.revision.saturating_add(1)),
        )?;
        record_legacy_revision_row(&transaction, meta)?;
        let mut produced = meta.clone();
        produced.title.clone_from(&plan.title);
        produced.description.clone_from(&plan.description);
        produced.bytes = plan.bytes;
        produced.entry.clone_from(&plan.entry);
        produced.category.clone_from(&plan.category);
        produced.body_sha256.clone_from(&plan.body_sha256);
        produced.revision = meta.revision.saturating_add(1);
        record_revision_row(&transaction, &produced, acting_client_id)?;
        self.fanout_delivery(
            &transaction,
            &produced,
            if operation == "artifact.restore" {
                WebhookEvent::Restored
            } else {
                WebhookEvent::Updated
            },
            intent_id,
        )?;
        let changes = transaction
            .execute(
                "UPDATE artifact_durability_intents \
                 SET state = 'metadata_committed', updated_at = datetime('now') WHERE id = ?1",
                params![intent_id],
            )
            .map_err(|error| sql_error("advance update durability intent", &error))?;
        if changes != 1 {
            return Err(AppError::Internal);
        }
        // A fault here drops the transaction unread, which is exactly what a crash before COMMIT
        // leaves behind.
        self.bare_fault(FaultPoint::UpdateCommitTransaction)?;
        transaction
            .commit()
            .map_err(|error| sql_error("commit update transaction", &error))?;
        Ok(true)
    }
}
