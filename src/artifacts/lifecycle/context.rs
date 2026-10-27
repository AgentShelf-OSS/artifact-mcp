//! Shared `StoreContext` helpers: delivery envelopes, audit receipts, faults, body paths,
//! and body staging.

use super::*;

impl StoreContext {
    pub(super) fn delivery_envelope(
        &self,
        meta: &ArtifactMeta,
        event: WebhookEvent,
    ) -> Result<DeliveryEnvelopeV1, AppError> {
        let subject = format!("artifact:{}:{}", meta.id, meta.revision);
        let event_id = stable_delivery_event_id(&meta.org, &event, &subject);
        let preview = (!meta.is_bundle
            && matches!(
                &event,
                WebhookEvent::Published | WebhookEvent::Updated | WebhookEvent::Restored
            ))
        .then(|| DeliveryPreviewReferenceV1::new(&meta.id.0, meta.revision, &meta.body_sha256))
        .transpose()?;
        DeliveryEnvelopeV1::build_with_preview(
            event_id,
            &meta.org,
            &event,
            &NotificationPayload {
                artifact_id: meta.id.clone(),
                title: meta.title.clone(),
                url: format!(
                    "{}/{}",
                    self.delivery_base_url.trim_end_matches('/'),
                    meta.id
                ),
                description: meta.description.clone(),
                uploader_label: meta.uploader_label.clone(),
                category: meta.category.clone(),
                revision: meta.revision,
                bytes: meta.bytes,
                viewer_email: None,
                body: None,
                resolver: None,
            },
            preview,
        )
    }

    pub(super) fn fanout_delivery(
        &self,
        transaction: &Transaction<'_>,
        meta: &ArtifactMeta,
        event: WebhookEvent,
        intent_id: &str,
    ) -> Result<(), AppError> {
        let envelope = self.delivery_envelope(meta, event.clone())?;
        outbox_fanout::fanout_in_transaction(
            transaction,
            &envelope,
            &meta.org,
            &event,
            Some(intent_id.to_owned()),
            SystemClock.now_unix_millis(),
            || self.ids.webhook_id().map(|id| id.0),
        )
        .map(|_| ())
    }

    pub(super) fn audit_event(
        operation: &str,
        target_id: &str,
        revision: Option<u64>,
    ) -> AuditEvent {
        AuditEvent {
            operation: operation.to_owned(),
            target_type: "artifact".to_owned(),
            target_id: target_id.to_owned(),
            result: "success".to_owned(),
            classification: String::new(),
            revision,
        }
    }

    pub(super) fn reserve_audit(
        &self,
        transaction: &rusqlite::Transaction<'_>,
        audit: &MutationAudit,
        intent_id: &str,
        operation: &str,
        target_id: &str,
        revision: Option<u64>,
    ) -> Result<(), AppError> {
        let event = Self::audit_event(operation, target_id, revision);
        let correlation = audit.correlation(operation, target_id, revision);
        if !reserve_receipt_in_transaction(
            transaction,
            &self.audit_key,
            &correlation,
            intent_id,
            audit.context(),
            &event,
        )? {
            return Err(AppError::Internal);
        }
        Ok(())
    }

    pub(super) fn finalize_audit_and_intent(
        &self,
        conn: &mut Connection,
        audit: &MutationAudit,
        intent_id: &str,
        operation: &str,
        target_id: &str,
        revision: Option<u64>,
    ) -> Result<(), AppError> {
        let key = &self.audit_key;
        let event = Self::audit_event(operation, target_id, revision);
        let correlation = audit.correlation(operation, target_id, revision);
        let transaction = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| sql_error("open audit finalization transaction", &error))?;
        let event_id = audit.event_id()?;
        let _ = finalize_receipt_in_transaction(
            &transaction,
            key,
            &correlation,
            &event_id,
            audit.context(),
            &event,
        )?;
        outbox::finalize_durability_success_in_transaction(
            &transaction,
            intent_id,
            SystemClock.now_unix_millis(),
        )?;
        transaction
            .commit()
            .map_err(|error| sql_error("commit audit finalization transaction", &error))
    }

    /// Finish a *proven* in-process compensation.  A pending receipt and its durability intent
    /// must both exist; otherwise this is an ambiguous/finalized state and must be left for
    /// reconciliation.  The safe failure event, receipt release, and intent release commit as
    /// one transaction, so no retry can relabel a partially released mutation.
    pub(super) fn cancel_compensated_audit_intent(
        &self,
        conn: &mut Connection,
        audit: &MutationAudit,
        intent_id: &str,
        operation: &str,
        target_id: &str,
        revision: Option<u64>,
    ) -> Result<(), AppError> {
        let event_id = audit.event_id()?;
        let event = AuditEvent {
            operation: operation.to_owned(),
            target_type: "artifact".to_owned(),
            target_id: target_id.to_owned(),
            result: "failure".to_owned(),
            classification: "compensated".to_owned(),
            revision,
        };
        let transaction = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| sql_error("open compensated audit cancellation", &error))?;
        verify_pending_receipt_in_transaction(&transaction, &self.audit_key, intent_id)?;
        append_in_transaction(
            &transaction,
            &self.audit_key,
            &event_id,
            audit.context(),
            &event,
        )?;
        delete_pending_receipt_in_transaction(&transaction, &self.audit_key, intent_id)?;
        outbox::compensate_durability_in_transaction(&transaction, intent_id)?;
        transaction
            .commit()
            .map_err(|error| sql_error("commit compensated audit cancellation", &error))
    }

    pub(super) fn rename_with_barrier(
        &self,
        from: &Path,
        to: &Path,
        point: FaultPoint,
    ) -> Result<(), AppError> {
        durable::rename_after_move(from, to, || {
            self.faults.check(point).map_err(InjectedFault::into_error)
        })
    }

    pub(super) fn fault(&self, point: FaultPoint) -> Result<(), Interrupt> {
        self.faults.check(point).map_err(Interrupt::from)
    }

    /// A fault at a boundary that sits outside any compensating `try`.
    pub(super) fn bare_fault(&self, point: FaultPoint) -> Result<(), AppError> {
        self.faults.check(point).map_err(InjectedFault::into_error)
    }

    fn single_body_path(&self, id: &SafeArtifactId) -> PathBuf {
        paths::single_body_path(&self.artifact_dir, id)
    }

    pub(super) fn bundle_dir(&self, id: &SafeArtifactId) -> PathBuf {
        paths::bundle_dir(&self.artifact_dir, id)
    }

    pub(super) fn body_path(&self, id: &SafeArtifactId, is_bundle: bool) -> PathBuf {
        paths::body_path(&self.artifact_dir, id, is_bundle)
    }

    /// `transientPath(id, kind)` — the random suffix is a fresh artifact-alphabet nanoid.
    /// [lib/store.js:178-180]
    pub(super) fn transient_path(
        &self,
        id: &SafeArtifactId,
        kind: TransientKind,
    ) -> Result<PathBuf, AppError> {
        let token = self.ids.artifact_id()?;
        paths::transient_path(&self.artifact_dir, id, kind, &token.0).ok_or(AppError::Internal)
    }

    /// `nextId()` — [lib/store.js:182-188]
    ///
    /// Node loops forever; a bounded retry keeps a broken id source from hanging a request. The
    /// chance of 64 consecutive collisions over a 33^12 space is not physically meaningful.
    pub(super) fn next_id(&self, conn: &Connection) -> Result<SafeArtifactId, AppError> {
        const MAX_ATTEMPTS: usize = 64;
        for _ in 0..MAX_ATTEMPTS {
            let candidate = self.ids.artifact_id()?;
            if is_reserved_artifact_id(&candidate.0) {
                continue;
            }
            let Some(id) = SafeArtifactId::parse(&candidate.0) else {
                continue;
            };
            if path_exists(&self.single_body_path(&id)) || path_exists(&self.bundle_dir(&id)) {
                continue;
            }
            if load_meta(conn, id.as_str())?.is_some() {
                continue;
            }
            return Ok(id);
        }
        tracing::error!("exhausted artifact id attempts");
        Err(AppError::Internal)
    }

    /// `files.writeFileSync(staging, html, "utf8")` — [lib/store.js:217], [lib/store.js:376]
    pub(super) fn stage_single_body(
        &self,
        staging: &Path,
        html: &str,
        file_sync: FaultPoint,
        directory_sync: FaultPoint,
    ) -> Result<(), AppError> {
        if let Some(parent) = staging.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| io_failure("create staging parent", staging, &error))?;
        }
        std::fs::write(staging, html.as_bytes())
            .map_err(|error| io_failure("write staged body", staging, &error))?;
        durable::sync_file_with_barrier(staging, || self.bare_fault(file_sync))?;
        if let Some(parent) = staging.parent() {
            durable::sync_dir_with_barrier(parent, || self.bare_fault(directory_sync))?;
        }
        Ok(())
    }

    /// ```text
    /// for (const [rel, content] of clean) {
    ///   const full = path.join(staging, rel);
    ///   files.mkdirSync(path.dirname(full), { recursive: true });
    ///   files.writeFileSync(full, content, "utf8");
    /// }
    /// ```
    /// — [lib/store.js:263-267], [lib/store.js:379-383]
    ///
    /// Files are written in the caller's order — the order the publisher supplied, and the order
    /// entry auto-selection already used (contract delta 4). Nothing here re-sorts them, and a
    /// duplicate relative path keeps Node's "last write wins".
    ///
    /// See contract-delta request 2 for the trailing-separator rejection.
    pub(super) fn stage_bundle_files(
        &self,
        staging: &Path,
        files: &[(String, String)],
        file_sync: FaultPoint,
        directory_sync: FaultPoint,
    ) -> Result<(), AppError> {
        std::fs::create_dir_all(staging)
            .map_err(|error| io_failure("create staging directory", staging, &error))?;
        for (rel, content) in files {
            if rel.ends_with('/') {
                // Node's `writeFileSync` on a trailing-slash path fails with EISDIR; `Path::join`
                // would silently drop the separator and create a file instead.
                tracing::error!(path = %rel, "bundle file name resolves to a directory");
                return Err(AppError::Internal);
            }
            let Some(full) = validation::contained_path(staging, rel) else {
                tracing::error!(path = %rel, "bundle file escaped its staging root");
                return Err(AppError::Internal);
            };
            if let Some(parent) = full.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|error| io_failure("create bundle directory", parent, &error))?;
            }
            std::fs::write(&full, content.as_bytes())
                .map_err(|error| io_failure("write bundle file", &full, &error))?;
        }
        durable::sync_tree_with_barrier(staging, || self.bare_fault(file_sync))?;
        if let Some(parent) = staging.parent() {
            durable::sync_dir_with_barrier(parent, || self.bare_fault(directory_sync))?;
        }
        Ok(())
    }
}
