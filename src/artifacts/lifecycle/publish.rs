//! Publish: stage the body, insert the row, rename the body into place.

use super::*;

impl StoreContext {
    // -----------------------------------------------------------------------
    // publish
    // -----------------------------------------------------------------------

    /// `publish` / `publishBundle` — [lib/store.js:206-281]
    pub(super) fn publish_sync(
        &self,
        conn: &mut Connection,
        request: PublishArtifact,
        audit: MutationAudit,
    ) -> Result<PublishedArtifact, AppError> {
        let _gate = self
            .lifecycle_gate
            .write()
            .map_err(|_| AppError::Internal)?;
        let prepared = self.prepare_publish(&request.content)?;
        // A publisher administrator may select a tenant other than its own.  Audit under the
        // artifact's persisted/target tenant while preserving the verified actor identity.
        let audit_tenant = if request.target_org.0.is_empty() {
            OrgId(DEFAULT_ORG.to_owned())
        } else {
            request.target_org.clone()
        };
        let audit = audit.for_affected_tenant(&audit_tenant);

        let id = self.next_id(conn)?;
        let staging = self.transient_path(&id, TransientKind::Staging)?;
        let final_path = self.body_path(&id, prepared.is_bundle);

        let intent_id = format!("publish:{}", id.as_str());
        start_durability_intent(
            conn,
            &intent_id,
            id.as_str(),
            "publish",
            &prepared.body_sha256,
            "",
            &staging,
        )?;
        let mut state = PublishState {
            inserted: false,
            final_durable: false,
        };
        let attempt = self.publish_attempt(
            conn,
            &request,
            &audit,
            PublishPlan {
                id: &id,
                staging: &staging,
                final_path: &final_path,
                bytes: prepared.bytes,
                body_sha256: &prepared.body_sha256,
                is_bundle: prepared.is_bundle,
                entry: &prepared.entry,
                single: prepared.single.as_deref(),
                bundle: prepared.bundle.as_ref(),
            },
            &mut state,
        );

        match attempt {
            Ok(meta) => Ok(PublishedArtifact {
                meta,
                file_count: prepared.file_count,
            }),
            Err(interrupt) => {
                if interrupt.compensate {
                    // `if (inserted) deleteById.run(id); safeRemove(staging); safeRemove(final);`
                    // — [lib/store.js:222-227], [lib/store.js:275-280]
                    if state.inserted {
                        self.bare_fault(FaultPoint::PublishDeleteRow)?;
                        conn.execute("DELETE FROM artifacts WHERE id = ?1", params![id.as_str()])
                            .map_err(|error| sql_error("delete published artifact", &error))?;
                    }
                    safe_remove(&staging);
                    if state.final_durable {
                        safe_remove(&final_path);
                    }
                    if state.inserted {
                        // Once the inserted row/body have been proven removed, record the safe
                        // terminal failure while releasing the reservation and intent atomically.
                        self.cancel_compensated_audit_intent(
                            conn,
                            &audit,
                            &intent_id,
                            "artifact.publish",
                            id.as_str(),
                            Some(1),
                        )?;
                    } else {
                        // Staging failed before a receipt was admitted.
                        finish_durability_intent(conn, &intent_id)?;
                    }
                }
                Err(interrupt.error)
            }
        }
    }

    /// Validation and digesting, before any side effect. [lib/store.js:207-211],
    /// [lib/store.js:231-260]
    fn prepare_publish(&self, content: &ArtifactContent) -> Result<PreparedPublish, AppError> {
        match content {
            ArtifactContent::SingleHtml(html) => {
                let validated = validation::validate_single_body(html, &self.limits)?;
                Ok(PreparedPublish {
                    bytes: validated.bytes,
                    body_sha256: validated.body_sha256,
                    is_bundle: false,
                    entry: String::new(),
                    file_count: None,
                    single: Some(html.clone()),
                    bundle: None,
                })
            }
            ArtifactContent::Bundle { files, entry } => {
                // `publishBundle` passes no `preferEntry`, so auto-selection takes the FIRST
                // `.html` in publisher order. [lib/store.js:252-255]
                let validated =
                    validation::validate_bundle(files, entry.as_deref(), None, &self.limits)?;
                let body_sha256 = bundle_manifest_digest(&validated.files);
                Ok(PreparedPublish {
                    bytes: validated.total_bytes,
                    body_sha256,
                    is_bundle: true,
                    entry: validated.entry.clone(),
                    file_count: Some(validated.files.len()),
                    single: None,
                    bundle: Some(validated),
                })
            }
        }
    }

    pub(super) fn publish_attempt(
        &self,
        conn: &mut Connection,
        request: &PublishArtifact,
        audit: &MutationAudit,
        plan: PublishPlan<'_>,
        state: &mut PublishState,
    ) -> Result<ArtifactMeta, Interrupt> {
        self.stage_publish_body(&plan)?;

        self.fault(FaultPoint::PublishInsert)?;
        let org = if request.target_org.0.is_empty() {
            DEFAULT_ORG
        } else {
            request.target_org.0.as_str()
        };
        // Ownership is server-controlled.  The MCP request contains no owner field; read the
        // current authenticated key row while publishing and snapshot that nullable identity.
        // A later key reassignment therefore cannot transfer historical artifacts.
        let owner_email: Option<String> = conn
            .query_row(
                "SELECT owner_email FROM api_keys WHERE client_id = ?1 AND org = ?2",
                params![request.publisher.client_id.0, request.publisher.org.0],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| Interrupt::from(sql_error("read publisher key owner", &error)))?
            .flatten();
        let transaction = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| Interrupt::from(sql_error("open publish transaction", &error)))?;
        transaction.execute(
            "INSERT INTO artifacts \
             (id, client_id, org, owner_email, uploader_label, title, description, bytes, is_bundle, entry, \
              category, body_sha256, artifact_key) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            params![
                plan.id.as_str(),
                request.publisher.client_id.0,
                org,
                owner_email,
                normalize_label(&request.publisher.label),
                normalize_title(request.title.as_deref()),
                normalize_description(request.description.as_deref()),
                as_i64(plan.bytes)?,
                i64::from(plan.is_bundle),
                plan.entry,
                normalize_category(request.category.as_deref().unwrap_or_default()),
                plan.body_sha256,
                request.artifact_key,
            ],
        )
        .map_err(|error| {
            if is_artifact_key_conflict(&error) {
                Interrupt::from(AppError::Conflict(ARTIFACT_KEY_TAKEN.to_owned()))
            } else {
                Interrupt::from(sql_error("insert artifact", &error))
            }
        })?;
        let meta = load_meta_unchecked(&transaction, plan.id.as_str())?
            .ok_or_else(|| Interrupt::from(AppError::Internal))?;
        record_revision_row(&transaction, &meta, Some(&request.publisher.client_id))?;
        self.reserve_audit(
            &transaction,
            audit,
            &format!("publish:{}", plan.id.as_str()),
            "artifact.publish",
            plan.id.as_str(),
            Some(1),
        )?;
        self.fanout_delivery(
            &transaction,
            &meta,
            WebhookEvent::Published,
            &format!("publish:{}", plan.id.as_str()),
        )?;
        transaction
            .commit()
            .map_err(|error| Interrupt::from(sql_error("commit publish transaction", &error)))?;
        state.inserted = true;
        advance_durability_intent(
            conn,
            &format!("publish:{}", plan.id.as_str()),
            "metadata_committed",
        )?;

        self.finish_publish_body(conn, audit, &plan, state)?;
        Ok(meta)
    }

    fn stage_publish_body(&self, plan: &PublishPlan<'_>) -> Result<(), Interrupt> {
        self.fault(FaultPoint::PublishStageWrite)?;
        if let Some(html) = plan.single {
            self.stage_single_body(
                plan.staging,
                html,
                FaultPoint::PublishStageFileSync,
                FaultPoint::PublishStageDirectorySync,
            )?;
        } else if let Some(bundle) = plan.bundle {
            self.stage_bundle_files(
                plan.staging,
                &bundle.files,
                FaultPoint::PublishStageFileSync,
                FaultPoint::PublishStageDirectorySync,
            )?;
        }
        Ok(())
    }

    fn finish_publish_body(
        &self,
        conn: &mut Connection,
        audit: &MutationAudit,
        plan: &PublishPlan<'_>,
        state: &mut PublishState,
    ) -> Result<(), Interrupt> {
        self.fault(FaultPoint::PublishRename)?;
        if let Err(error) = self.rename_with_barrier(
            plan.staging,
            plan.final_path,
            FaultPoint::PublishRenameBarrier,
        ) {
            if path_exists(plan.final_path) && !path_exists(plan.staging) {
                state.final_durable = true;
                return Err(Interrupt {
                    error,
                    compensate: false,
                });
            }
            return Err(Interrupt::from(error));
        }
        state.final_durable = true;
        advance_durability_intent(
            conn,
            &format!("publish:{}", plan.id.as_str()),
            "body_durable",
        )?;
        self.fault(FaultPoint::PublishComplete)?;
        self.finalize_audit_and_intent(
            conn,
            audit,
            &format!("publish:{}", plan.id.as_str()),
            "artifact.publish",
            plan.id.as_str(),
            Some(1),
        )?;
        Ok(())
    }
}

/// A validated publish request: the body is digested and the entry resolved, nothing is written.
struct PreparedPublish {
    bytes: u64,
    body_sha256: String,
    is_bundle: bool,
    entry: String,
    file_count: Option<usize>,
    single: Option<String>,
    bundle: Option<ValidatedBundle>,
}
