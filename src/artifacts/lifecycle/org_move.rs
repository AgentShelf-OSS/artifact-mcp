//! Move an artifact and its org-bearing children to another organization.

use super::*;

impl StoreContext {
    /// `moveArtifactToOrg(id, targetOrg, category)` — [lib/store.js:463-480]
    ///
    /// One transaction with deferred foreign keys so the parent and every org-bearing child move
    /// together; public shares are REVOKED rather than carried into the new tenant, and
    /// `client_id` deliberately stays put so the old org-locked key can no longer update it.
    pub(super) fn move_to_org_sync(
        &self,
        conn: &mut Connection,
        id: &str,
        target_org: &str,
        category: Option<&str>,
        audit: MutationAudit,
    ) -> Result<ArtifactMeta, AppError> {
        let _gate = self
            .lifecycle_gate
            .write()
            .map_err(|_| AppError::Internal)?;
        let Some(meta) = load_meta(conn, id)? else {
            return Err(AppError::NotFound("not_found".to_owned()));
        };
        let org = js_trim(target_org).to_owned();
        if !org_exists(conn, &org)? {
            return Err(AppError::Validation(format!(
                "Unknown organization \"{org}\"."
            )));
        }
        let next_category = category.map_or_else(|| meta.category.clone(), normalize_category);

        self.bare_fault(FaultPoint::MoveTransaction)?;
        let transaction = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| sql_error("open move transaction", &error))?;
        let event_id = audit.event_id()?;
        // Audit a cross-org move under the source/affected tenant.  The target organization is
        // intentionally absent: tenant scope and the count-only revocation classification are
        // enough for an authorized investigation without retaining share identifiers.
        let scoped_audit = audit.for_affected_tenant(&meta.org);
        // Composite FKs are checked at COMMIT, after the parent and all children have moved.
        transaction
            .execute_batch("PRAGMA defer_foreign_keys = ON")
            .map_err(|error| sql_error("defer foreign keys", &error))?;
        if org != meta.org.0 {
            // A Discord connection, external thread, and inbound policy belong to the source
            // organization. Preserve the artifact's local feedback, but revoke those bindings
            // before re-tenanting it. Immutable provider outbox rows remain as delivery history.
            revoke_cross_org_bindings(&transaction, id)?;
        }
        transaction
            .execute(
                "UPDATE artifacts SET org = ?1, category = ?2, updated_at = datetime('now') \
                 WHERE id = ?3",
                params![org, next_category, id],
            )
            .map_err(|error| sql_error("move artifact", &error))?;
        move_org_bearing_children(&transaction, &org, id)?;
        let revoked_shares = transaction
            .execute(
                "DELETE FROM artifact_shares WHERE artifact_id = ?1",
                params![id],
            )
            .map_err(|error| sql_error("revoke shares on move", &error))?;
        let event = AuditEvent {
            operation: "artifact.org.move".to_owned(),
            target_type: "artifact".to_owned(),
            target_id: id.to_owned(),
            result: "success".to_owned(),
            classification: format!("shares_revoked_{revoked_shares}"),
            revision: Some(meta.revision),
        };
        append_in_transaction(
            &transaction,
            &self.audit_key,
            &event_id,
            scoped_audit.context(),
            &event,
        )?;
        transaction
            .commit()
            .map_err(|error| sql_error("commit move transaction", &error))?;

        load_meta(conn, id)?.ok_or(AppError::Internal)
    }
}

fn revoke_cross_org_bindings(transaction: &Transaction<'_>, id: &str) -> Result<(), AppError> {
    for (table, operation) in [
        (
            "discord_inbound_message_state",
            "revoke Discord inbound message state on move",
        ),
        (
            "artifact_discord_inbound_policies",
            "revoke Discord inbound policy on move",
        ),
        (
            "discussion_message_links",
            "revoke Discord message links on move",
        ),
        (
            "discord_notification_anchor_recoveries",
            "revoke Discord notification recovery on move",
        ),
        (
            "artifact_discussion_overrides",
            "revoke Discord discussion override on move",
        ),
        (
            "artifact_discussions",
            "revoke Discord discussion binding on move",
        ),
    ] {
        transaction
            .execute(&format!("DELETE FROM {table} WHERE artifact_id = ?1"), [id])
            .map_err(|error| sql_error(operation, &error))?;
    }
    transaction
        .execute(
            "UPDATE artifacts SET artifact_key = NULL WHERE id = ?1",
            [id],
        )
        .map_err(|error| sql_error("clear artifact key on move", &error))?;
    Ok(())
}

fn move_org_bearing_children(
    transaction: &Transaction<'_>,
    org: &str,
    id: &str,
) -> Result<(), AppError> {
    for (sql, operation) in [
        (
            "UPDATE feedback SET org = ?1 WHERE artifact_id = ?2",
            "move feedback",
        ),
        (
            "UPDATE artifact_revisions SET org = ?1 WHERE artifact_id = ?2",
            "move revisions",
        ),
        (
            "UPDATE artifact_views SET org = ?1 WHERE artifact_id = ?2",
            "move views",
        ),
    ] {
        transaction
            .execute(sql, params![org, id])
            .map_err(|error| sql_error(operation, &error))?;
    }
    Ok(())
}
