//! `impl ArtifactService for ArtifactStore`.

use super::*;

// ---------------------------------------------------------------------------
// ArtifactService
// ---------------------------------------------------------------------------

impl ArtifactService for ArtifactStore {
    fn find_meta<'a>(
        &'a self,
        id: &'a ArtifactId,
    ) -> BoxFuture<'a, Result<Option<ArtifactMeta>, AppError>> {
        let id = id.0.clone();
        Box::pin(async move {
            // `getArtifactMeta: (id) => isReserved(id) ? null : getMetaStmt.get(id) || null`
            // — [lib/store.js:793]
            if is_reserved_artifact_id(&id) {
                return Ok(None);
            }
            db::interact(&self.pool, move |conn| load_meta(conn, &id)).await
        })
    }

    fn find_meta_by_key<'a>(
        &'a self,
        org: &'a OrgId,
        artifact_key: &'a str,
    ) -> BoxFuture<'a, Result<Option<ArtifactMeta>, AppError>> {
        let org = org.0.clone();
        let artifact_key = artifact_key.to_owned();
        Box::pin(async move {
            db::interact(&self.pool, move |conn| {
                load_meta_by_key(conn, &org, &artifact_key)
            })
            .await
        })
    }

    fn publish(
        &self,
        request: PublishArtifact,
        audit: MutationAudit,
    ) -> BoxFuture<'_, Result<PublishedArtifact, AppError>> {
        let context = self.context();
        let store = self.clone();
        Box::pin(async move {
            let published = db::interact(&self.pool, move |conn| {
                context.publish_sync(conn, request, audit)
            })
            .await?;
            store.schedule_preview(published.meta.clone());
            Ok(published)
        })
    }

    fn list_for_publisher<'a>(
        &'a self,
        publisher: &'a PublisherIdentity,
    ) -> BoxFuture<'a, Result<Vec<ArtifactMeta>, AppError>> {
        let client_id = publisher.client_id.0.clone();
        let org = (!publisher.is_admin()).then(|| publisher.org.0.clone());
        Box::pin(async move {
            db::interact(&self.pool, move |conn| match org {
                // `listByClient` — [lib/store.js:112]
                None => list_metas(
                    conn,
                    &format!(
                        "SELECT {META_COLUMNS} FROM artifacts WHERE client_id = ?1 \
                         ORDER BY created_at DESC"
                    ),
                    &[&client_id],
                ),
                // `listByClientOrg` — an org-locked key must never keep listing artifacts moved
                // to another tenant. [lib/store.js:115]
                Some(org) => list_metas(
                    conn,
                    &format!(
                        "SELECT {META_COLUMNS} FROM artifacts WHERE client_id = ?1 AND org = ?2 \
                         ORDER BY created_at DESC"
                    ),
                    &[&client_id, &org],
                ),
            })
            .await
        })
    }

    fn list_org_artifacts<'a>(
        &'a self,
        org: &'a OrgId,
        include_hidden: bool,
    ) -> BoxFuture<'a, Result<Vec<ArtifactMeta>, AppError>> {
        let org = org.0.clone();
        Box::pin(async move {
            db::interact(&self.pool, move |conn| {
                // `listByOrg` / `listByOrgIncludingHidden` — [lib/store.js:116-117]
                let filter = if include_hidden {
                    ""
                } else {
                    " AND hidden = 0"
                };
                list_metas(
                    conn,
                    &format!(
                        "SELECT {META_COLUMNS} FROM artifacts WHERE org = ?1{filter} \
                         ORDER BY client_id ASC, created_at DESC"
                    ),
                    &[&org],
                )
            })
            .await
        })
    }

    fn list_all_grouped_by_org(
        &self,
        include_hidden: bool,
    ) -> BoxFuture<'_, Result<Vec<OrgArtifacts>, AppError>> {
        Box::pin(async move {
            db::interact(&self.pool, move |conn| {
                // `listAll` / `listAllVisible`, grouped by org in first-seen order.
                // [lib/store.js:118-119], [lib/store.js:61-69]
                let filter = if include_hidden {
                    ""
                } else {
                    " WHERE hidden = 0"
                };
                let rows = list_metas(
                    conn,
                    &format!(
                        "SELECT {META_COLUMNS} FROM artifacts{filter} \
                         ORDER BY org ASC, client_id ASC, created_at DESC"
                    ),
                    &[],
                )?;
                let mut grouped: Vec<OrgArtifacts> = Vec::new();
                for row in rows {
                    match grouped.last_mut() {
                        Some(group) if group.org == row.org => group.items.push(row),
                        _ => grouped.push(OrgArtifacts {
                            org: row.org.clone(),
                            items: vec![row],
                        }),
                    }
                }
                Ok(grouped)
            })
            .await
        })
    }

    fn list_org_ids<'a>(
        &'a self,
        org: &'a OrgId,
        include_hidden: bool,
    ) -> BoxFuture<'a, Result<Vec<ArtifactId>, AppError>> {
        let org = org.0.clone();
        Box::pin(async move {
            db::interact(&self.pool, move |conn| {
                // `listIdsByOrg` / `listIdsByOrgIncludingHidden` — [lib/store.js:109-110]
                let filter = if include_hidden {
                    ""
                } else {
                    " AND hidden = 0"
                };
                let mut statement = conn
                    .prepare(&format!(
                        "SELECT id FROM artifacts WHERE org = ?1{filter} \
                         AND NOT EXISTS (SELECT 1 FROM artifact_durability_intents i \
                                         WHERE i.artifact_id = artifacts.id) \
                         ORDER BY created_at DESC, id DESC"
                    ))
                    .map_err(|error| sql_error("prepare org id list", &error))?;
                statement
                    .query_map(params![org], |row| row.get::<_, String>(0).map(ArtifactId))
                    .map_err(|error| sql_error("query org id list", &error))?
                    .collect::<rusqlite::Result<Vec<_>>>()
                    .map_err(|error| sql_error("read org id list", &error))
            })
            .await
        })
    }

    fn read_body<'a>(
        &'a self,
        artifact: &'a AuthorizedArtifact,
    ) -> BoxFuture<'a, Result<Option<ArtifactFile>, AppError>> {
        Box::pin(self.read_body_for(artifact.meta()))
    }

    fn read_current_thumbnail<'a>(
        &'a self,
        artifact: &'a AuthorizedArtifact,
        digest: &'a str,
        previews: Arc<dyn crate::ports::PreviewService>,
    ) -> BoxFuture<'a, Result<Option<Vec<u8>>, AppError>> {
        let context = self.context();
        let id = artifact.meta().id.0.clone();
        let digest = digest.to_owned();
        let authorized_org = artifact.meta().org.clone();
        let authorized_client_id = artifact.meta().client_id.clone();
        Box::pin(async move {
            db::interact(&self.pool, move |conn| {
                let _gate = context
                    .lifecycle_gate
                    .read()
                    .map_err(|_| AppError::Internal)?;
                let Some(current) = load_meta(conn, &id)? else {
                    return Ok(None);
                };
                // The original grant is tied to its tenant and (for publisher authors) client
                // identity. A re-tenant between authorization and this guarded recheck must
                // not let an old grant read an unchanged-digest thumbnail.
                if current.id.0 != id
                    || current.body_sha256 != digest
                    || current.org != authorized_org
                    || current.client_id != authorized_client_id
                {
                    return Ok(None);
                }
                previews.read_thumbnail_sync(&current, &digest)
            })
            .await
        })
    }

    fn read_bundle_file<'a>(
        &'a self,
        artifact: &'a AuthorizedArtifact,
        relative_path: &'a str,
    ) -> BoxFuture<'a, Result<Option<ArtifactFile>, AppError>> {
        Box::pin(self.read_bundle_file_for(artifact.meta(), relative_path))
    }

    fn read_revision_body<'a>(
        &'a self,
        artifact: &'a AuthorizedArtifact,
        revision: u64,
        relative_path: Option<&'a str>,
    ) -> BoxFuture<'a, Result<Option<ArtifactFile>, AppError>> {
        Box::pin(self.read_revision_body_for(artifact.meta(), revision, relative_path))
    }

    fn list_bundle_files<'a>(
        &'a self,
        artifact: &'a AuthorizedArtifact,
        revision: Option<u64>,
    ) -> BoxFuture<'a, Result<Option<Vec<(String, u64)>>, AppError>> {
        Box::pin(self.list_bundle_files_for(artifact.meta(), revision))
    }

    fn list_revisions<'a>(
        &'a self,
        artifact: &'a AuthorizedArtifact,
    ) -> BoxFuture<'a, Result<RevisionHistory, AppError>> {
        Box::pin(self.list_revisions_for(artifact.meta()))
    }

    fn update(
        &self,
        artifact: AuthorizedArtifact,
        update: ArtifactUpdate,
        audit: MutationAudit,
    ) -> BoxFuture<'_, Result<UpdateArtifactResult, AppError>> {
        Box::pin(async move { self.update_for(artifact.meta(), update, audit).await })
    }

    fn restore(
        &self,
        artifact: AuthorizedArtifact,
        revision: u64,
        acting_client_id: Option<ClientId>,
        audit: MutationAudit,
    ) -> BoxFuture<'_, Result<RestoreArtifactResult, AppError>> {
        Box::pin(async move {
            self.restore_for(artifact.meta(), revision, acting_client_id, audit)
                .await
        })
    }

    fn delete(
        &self,
        artifact: AuthorizedArtifact,
        audit: MutationAudit,
    ) -> BoxFuture<'_, Result<bool, AppError>> {
        Box::pin(async move { self.delete_for(artifact.meta(), audit).await })
    }

    fn set_category(
        &self,
        artifact: AuthorizedArtifact,
        category: String,
        audit: MutationAudit,
    ) -> BoxFuture<'_, Result<ArtifactMeta, AppError>> {
        Box::pin(async move {
            self.set_category_for(artifact.meta(), &category, audit)
                .await
        })
    }

    fn set_hidden(
        &self,
        artifact: AuthorizedArtifact,
        hidden: bool,
        audit: MutationAudit,
    ) -> BoxFuture<'_, Result<ArtifactMeta, AppError>> {
        Box::pin(async move { self.set_hidden_for(artifact.meta(), hidden, audit).await })
    }

    fn move_to_org(
        &self,
        artifact: AuthorizedArtifact,
        target_org: OrgId,
        category: Option<String>,
        audit: MutationAudit,
    ) -> BoxFuture<'_, Result<ArtifactMeta, AppError>> {
        Box::pin(async move {
            self.move_to_org_for(artifact.meta(), &target_org.0, category.as_deref(), audit)
                .await
        })
    }

    fn audit_storage(
        &self,
        clean_transient: bool,
    ) -> BoxFuture<'_, Result<StorageAuditReport, AppError>> {
        let context = self.context();
        Box::pin(async move {
            db::interact(&self.pool, move |conn| {
                let _gate = context
                    .lifecycle_gate
                    .write()
                    .map_err(|_| AppError::Internal)?;
                reconciliation::audit_storage(
                    conn,
                    &context.artifact_dir,
                    clean_transient,
                    context.faults.as_ref(),
                    &context.audit_key,
                )
            })
            .await
        })
    }

    fn backfill_body_digests(&self) -> BoxFuture<'_, Result<DigestBackfillReport, AppError>> {
        let context = self.context();
        Box::pin(async move {
            db::interact(&self.pool, move |conn| {
                let _gate = context
                    .lifecycle_gate
                    .write()
                    .map_err(|_| AppError::Internal)?;
                reconciliation::backfill_body_digests(
                    conn,
                    &context.artifact_dir,
                    context.faults.as_ref(),
                )
            })
            .await
        })
    }
}
