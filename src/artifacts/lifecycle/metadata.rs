//! Metadata-only mutations (category, visibility) and the revision listing.

use super::*;

impl StoreContext {
    // -----------------------------------------------------------------------
    // metadata-only mutations
    // -----------------------------------------------------------------------

    /// `setCategory(id, category)` — [lib/store.js:446-452]
    pub(super) fn set_category_sync(
        &self,
        conn: &mut Connection,
        id: &str,
        category: &str,
        audit: MutationAudit,
    ) -> Result<ArtifactMeta, AppError> {
        let _gate = self
            .lifecycle_gate
            .write()
            .map_err(|_| AppError::Internal)?;
        let Some(meta) = load_meta(conn, id)? else {
            return Err(AppError::NotFound("not_found".to_owned()));
        };
        self.bare_fault(FaultPoint::MetadataWrite)?;
        let event_id = audit.event_id()?;
        let event = Self::audit_event("artifact.category.set", id, Some(meta.revision));
        let scoped_audit = audit.for_affected_tenant(&meta.org);
        let transaction = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| sql_error("open category audit transaction", &error))?;
        transaction
            .execute(
                "UPDATE artifacts SET category = ?1, updated_at = datetime('now') WHERE id = ?2",
                params![normalize_category(category), id],
            )
            .map_err(|error| sql_error("set artifact category", &error))?;
        append_in_transaction(
            &transaction,
            &self.audit_key,
            &event_id,
            scoped_audit.context(),
            &event,
        )?;
        transaction
            .commit()
            .map_err(|error| sql_error("commit category audit transaction", &error))?;
        load_meta(conn, id)?.ok_or(AppError::Internal)
    }

    /// `setHidden(id, hidden)` — hidden is an unlisted flag, never an access boundary.
    /// [lib/store.js:455-461]
    pub(super) fn set_hidden_sync(
        &self,
        conn: &mut Connection,
        id: &str,
        hidden: bool,
        audit: MutationAudit,
    ) -> Result<ArtifactMeta, AppError> {
        let _gate = self
            .lifecycle_gate
            .write()
            .map_err(|_| AppError::Internal)?;
        let Some(meta) = load_meta(conn, id)? else {
            return Err(AppError::NotFound("not_found".to_owned()));
        };
        self.bare_fault(FaultPoint::MetadataWrite)?;
        let event_id = audit.event_id()?;
        let event = AuditEvent {
            operation: "artifact.visibility.set".to_owned(),
            target_type: "artifact".to_owned(),
            target_id: id.to_owned(),
            result: "success".to_owned(),
            classification: if hidden { "hidden" } else { "shared" }.to_owned(),
            revision: Some(meta.revision),
        };
        let scoped_audit = audit.for_affected_tenant(&meta.org);
        let transaction = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| sql_error("open visibility audit transaction", &error))?;
        transaction
            .execute(
                "UPDATE artifacts SET hidden = ?1, updated_at = datetime('now') WHERE id = ?2",
                params![i64::from(hidden), id],
            )
            .map_err(|error| sql_error("set artifact visibility", &error))?;
        append_in_transaction(
            &transaction,
            &self.audit_key,
            &event_id,
            scoped_audit.context(),
            &event,
        )?;
        transaction
            .commit()
            .map_err(|error| sql_error("commit visibility audit transaction", &error))?;
        load_meta(conn, id)?.ok_or(AppError::Internal)
    }

    /// `listRevisions(id)` — [lib/store.js:577-581]
    pub(super) fn list_revisions_sync(
        &self,
        conn: &Connection,
        meta: &ArtifactMeta,
    ) -> Result<RevisionHistory, AppError> {
        // Node reloads the live row before listing. Use that revision both for the response and
        // to exclude the live attribution marker from the retained-history array.
        let current = load_meta(conn, &meta.id.0)?.map_or(meta.revision, |row| row.revision);
        let mut statement = conn
            .prepare(&format!(
                "SELECT {REVISION_COLUMNS} FROM artifact_revisions \
                 WHERE artifact_id = ?1 AND revision < ?2 ORDER BY revision DESC"
            ))
            .map_err(|error| sql_error("prepare revision list", &error))?;
        let revisions = statement
            .query_map(params![meta.id.0, as_i64(current)?], revision_from_row)
            .map_err(|error| sql_error("query revision list", &error))?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|error| sql_error("read revision list", &error))?;
        Ok(RevisionHistory { current, revisions })
    }
}
