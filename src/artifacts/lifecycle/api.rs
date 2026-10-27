//! The metadata-addressed async `*_for` API on `ArtifactStore`.

use super::*;

// ---------------------------------------------------------------------------
// Metadata-addressed operations
// ---------------------------------------------------------------------------

/// The store's own API takes the artifact's [`ArtifactMeta`], not an [`AuthorizedArtifact`].
///
/// That mirrors the reference exactly — `lib/store.js` performs no authorization ("Authorization
/// is the caller's responsibility (route uses artifactAccess)", [lib/store.js:445]) — and it keeps
/// the wrapper what the contract says it is: a *caller-side* proof consumed by the frozen
/// [`ArtifactService`] surface below, which is the only path routes and MCP tools use. It also
/// lets U08's crash matrix drive every mutation before U06 lands the wrapper constructors.
impl ArtifactStore {
    /// The body half of `readArtifact(id)` — [lib/store.js:494-501]
    ///
    /// # Errors
    /// Returns [`AppError::Internal`] when the blocking task fails.
    pub async fn read_body_for(
        &self,
        meta: &ArtifactMeta,
    ) -> Result<Option<ArtifactFile>, AppError> {
        let context = self.context();
        let id = meta.id.0.clone();
        db::interact(&self.pool, move |conn| {
            let _gate = context
                .lifecycle_gate
                .read()
                .map_err(|_| AppError::Internal)?;
            // Authorization may have resolved just before a lifecycle marker was created. Recheck
            // in the same blocking closure immediately before filesystem access.
            if load_meta(conn, &id)?.is_none() {
                return Ok(None);
            }
            // `readArtifact` refuses a reserved id before touching the filesystem.
            // [lib/store.js:495]
            let Some(id) = SafeArtifactId::addressable(&id) else {
                return Ok(None);
            };
            Ok(read::read_body(&context.artifact_dir, &id))
        })
        .await
    }

    /// `readBundleFile(id, relPath)` — [lib/store.js:482-492]
    ///
    /// # Errors
    /// Returns [`AppError::Internal`] when the blocking task fails.
    pub async fn read_bundle_file_for(
        &self,
        meta: &ArtifactMeta,
        relative_path: &str,
    ) -> Result<Option<ArtifactFile>, AppError> {
        let context = self.context();
        let meta = meta.clone();
        let relative_path = relative_path.to_owned();
        db::interact(&self.pool, move |conn| {
            let _gate = context
                .lifecycle_gate
                .read()
                .map_err(|_| AppError::Internal)?;
            if load_meta(conn, &meta.id.0)?.is_none() {
                return Ok(None);
            }
            // `readBundleFile` returns null for a single-file artifact. [lib/store.js:484]
            if !meta.is_bundle {
                return Ok(None);
            }
            let Some(id) = SafeArtifactId::parse(&meta.id.0) else {
                return Ok(None);
            };
            Ok(read::read_bundle_file(
                &context.artifact_dir,
                &id,
                &meta.entry,
                Some(relative_path.as_str()),
            ))
        })
        .await
    }

    /// `readHistoryArtifact` (no path) versus `readHistoryBundleFile` (a path, possibly empty,
    /// which falls back to the revision's own entry).
    /// [lib/store.js:583-601], [lib/app.js:434], [lib/app.js:445]
    ///
    /// # Errors
    /// Propagates a database failure as [`AppError::Internal`].
    pub async fn read_revision_body_for(
        &self,
        meta: &ArtifactMeta,
        revision: u64,
        relative_path: Option<&str>,
    ) -> Result<Option<ArtifactFile>, AppError> {
        let context = self.context();
        let id = meta.id.0.clone();
        let relative_path = relative_path.map(ToOwned::to_owned);
        db::interact(&self.pool, move |conn| {
            let _gate = context
                .lifecycle_gate
                .read()
                .map_err(|_| AppError::Internal)?;
            if load_meta(conn, &id)?.is_none() {
                return Ok(None);
            }
            let Some(safe_id) = SafeArtifactId::parse(&id) else {
                return Ok(None);
            };
            let Some(row) = load_revision(conn, &id, revision)? else {
                return Ok(None);
            };
            Ok(match relative_path {
                None if row.is_bundle => None,
                None => read::read_revision_body(&context.artifact_dir, &safe_id, revision),
                Some(_) if !row.is_bundle => None,
                Some(relative) => read::read_revision_bundle_file(
                    &context.artifact_dir,
                    &safe_id,
                    revision,
                    &row.entry,
                    Some(relative.as_str()),
                ),
            })
        })
        .await
    }

    pub async fn list_bundle_files_for(
        &self,
        meta: &ArtifactMeta,
        revision: Option<u64>,
    ) -> Result<Option<Vec<(String, u64)>>, AppError> {
        let context = self.context();
        let id = meta.id.0.clone();
        let is_bundle = meta.is_bundle;
        db::interact(&self.pool, move |conn| {
            let _gate = context
                .lifecycle_gate
                .read()
                .map_err(|_| AppError::Internal)?;
            if load_meta(conn, &id)?.is_none() {
                return Ok(None);
            }
            let Some(safe_id) = SafeArtifactId::parse(&id) else {
                return Ok(None);
            };
            let root = if let Some(revision) = revision {
                let Some(row) = load_revision(conn, &id, revision)? else {
                    return Ok(None);
                };
                if !row.is_bundle {
                    return Ok(None);
                }
                paths::history_body_path(&context.artifact_dir, &safe_id, revision, true)
            } else {
                if !is_bundle {
                    return Ok(None);
                }
                paths::bundle_dir(&context.artifact_dir, &safe_id)
            };
            read::list_bundle_files(&root)
        })
        .await
    }

    /// `listRevisions(id)` — [lib/store.js:577-581]
    ///
    /// # Errors
    /// Propagates a database failure as [`AppError::Internal`].
    pub async fn list_revisions_for(
        &self,
        meta: &ArtifactMeta,
    ) -> Result<RevisionHistory, AppError> {
        let context = self.context();
        let meta = meta.clone();
        db::interact(&self.pool, move |conn| {
            let _gate = context
                .lifecycle_gate
                .read()
                .map_err(|_| AppError::Internal)?;
            if load_meta(conn, &meta.id.0)?.is_none() {
                return Err(AppError::ConcealedNotFound);
            }
            context.list_revisions_sync(conn, &meta)
        })
        .await
    }

    /// `update({ … })` — [lib/store.js:314-442]
    ///
    /// # Errors
    /// [`AppError::NotFound`] (`not_found`), [`AppError::Conflict`] (`conflict`),
    /// [`AppError::Validation`] with Node's message, or a propagated I/O failure.
    pub async fn update_for(
        &self,
        meta: &ArtifactMeta,
        update: ArtifactUpdate,
        audit: MutationAudit,
    ) -> Result<UpdateArtifactResult, AppError> {
        let context = self.context();
        let id = meta.id.0.clone();
        let updated = db::interact(&self.pool, move |conn| {
            context.update_sync(
                conn,
                &id,
                update.expected_revision,
                &update,
                &audit,
                "artifact.update",
            )
        })
        .await?;
        if updated.changed {
            self.schedule_preview(updated.meta.clone());
        }
        Ok(updated)
    }

    /// `restoreById(id, revision, …)` — [lib/store.js:605-624]
    ///
    /// # Errors
    /// [`AppError::NotFound`] (`not_found` / `revision_not_found`), [`AppError::Conflict`]
    /// (`type_mismatch`), or [`AppError::Gone`] (`body_missing`).
    pub async fn restore_for(
        &self,
        meta: &ArtifactMeta,
        revision: u64,
        acting_client_id: Option<ClientId>,
        audit: MutationAudit,
    ) -> Result<RestoreArtifactResult, AppError> {
        let context = self.context();
        let id = meta.id.0.clone();
        let restored = db::interact(&self.pool, move |conn| {
            context.restore_sync(conn, &id, revision, acting_client_id, audit)
        })
        .await?;
        self.schedule_preview(restored.meta.clone());
        Ok(restored)
    }

    /// `removeById(id, …)` — [lib/store.js:626-644]
    ///
    /// # Errors
    /// Propagates a database or filesystem failure after compensating.
    pub async fn delete_for(
        &self,
        meta: &ArtifactMeta,
        audit: MutationAudit,
    ) -> Result<bool, AppError> {
        let context = self.context();
        let id = meta.id.0.clone();
        db::interact(&self.pool, move |conn| {
            context.delete_sync(conn, &id, audit)
        })
        .await
    }

    /// `setCategory(id, category)` — [lib/store.js:446-452]
    ///
    /// # Errors
    /// [`AppError::NotFound`] (`not_found`) or a propagated database failure.
    pub async fn set_category_for(
        &self,
        meta: &ArtifactMeta,
        category: &str,
        audit: MutationAudit,
    ) -> Result<ArtifactMeta, AppError> {
        let context = self.context();
        let id = meta.id.0.clone();
        let category = category.to_owned();
        db::interact(&self.pool, move |conn| {
            context.set_category_sync(conn, &id, &category, audit)
        })
        .await
    }

    /// `setHidden(id, hidden)` — [lib/store.js:455-461]
    ///
    /// # Errors
    /// [`AppError::NotFound`] (`not_found`) or a propagated database failure.
    pub async fn set_hidden_for(
        &self,
        meta: &ArtifactMeta,
        hidden: bool,
        audit: MutationAudit,
    ) -> Result<ArtifactMeta, AppError> {
        let context = self.context();
        let id = meta.id.0.clone();
        db::interact(&self.pool, move |conn| {
            context.set_hidden_sync(conn, &id, hidden, audit)
        })
        .await
    }

    /// `moveArtifactToOrg(id, targetOrg, category)` — [lib/store.js:463-480]
    ///
    /// # Errors
    /// [`AppError::NotFound`] (`not_found`) or [`AppError::Validation`] with Node's
    /// `Unknown organization "…".` message.
    pub async fn move_to_org_for(
        &self,
        meta: &ArtifactMeta,
        target_org: &str,
        category: Option<&str>,
        audit: MutationAudit,
    ) -> Result<ArtifactMeta, AppError> {
        let context = self.context();
        let id = meta.id.0.clone();
        let target_org = target_org.to_owned();
        let category = category.map(ToOwned::to_owned);
        db::interact(&self.pool, move |conn| {
            context.move_to_org_sync(conn, &id, &target_org, category.as_deref(), audit)
        })
        .await
    }
}
