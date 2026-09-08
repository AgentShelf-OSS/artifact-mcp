//! Shared-pool viewer state adapter, including audited deletion.

use crate::{
    error::AppError,
    mcp::protocol::OrderedJson,
    model::EmailAddress,
    persistence::{
        db::{self, DbPool},
        state::{self, StateError, StateKey, StateScope, StateValue},
    },
    ports::{BoxFuture, state::ViewerStateService},
    security::{
        access::AuthorizedArtifact,
        audit::{self, AuditEvent, MutationAudit},
    },
};

pub struct SqliteViewerState {
    pool: DbPool,
    audit_key: Option<[u8; 32]>,
}

impl SqliteViewerState {
    #[must_use]
    pub const fn new(pool: DbPool, audit_key: Option<[u8; 32]>) -> Self {
        Self { pool, audit_key }
    }
}

impl ViewerStateService for SqliteViewerState {
    fn list(
        &self,
        artifact: AuthorizedArtifact,
        scope: StateScope,
        viewer: Option<EmailAddress>,
    ) -> BoxFuture<'_, Result<Vec<StateKey>, AppError>> {
        Box::pin(state::list_pooled(
            &self.pool,
            artifact.into_meta().id,
            scope,
            viewer.map(|v| v.0),
        ))
    }

    fn get(
        &self,
        artifact: AuthorizedArtifact,
        key: String,
        scope: StateScope,
        viewer: Option<EmailAddress>,
    ) -> BoxFuture<'_, Result<Option<StateValue>, AppError>> {
        Box::pin(state::get_pooled(
            &self.pool,
            artifact.into_meta().id,
            key,
            scope,
            viewer.map(|v| v.0),
        ))
    }

    fn put(
        &self,
        artifact: AuthorizedArtifact,
        key: String,
        value: OrderedJson,
        if_revision: Option<u64>,
        writer: EmailAddress,
        scope: StateScope,
    ) -> BoxFuture<'_, Result<StateValue, StateError>> {
        Box::pin(state::set_pooled(
            &self.pool,
            artifact.into_meta().id,
            key,
            value,
            if_revision,
            writer,
            scope,
        ))
    }

    fn delete(
        &self,
        artifact: AuthorizedArtifact,
        key: String,
        scope: StateScope,
        viewer: Option<EmailAddress>,
        mutation: MutationAudit,
    ) -> BoxFuture<'_, Result<(), AppError>> {
        let meta = artifact.into_meta();
        let audit_key = self.audit_key;
        Box::pin(async move {
            let mutation = mutation.for_target_tenant(&meta.org.0)?;
            db::interact(&self.pool, move |conn| {
                let tx = conn
                    .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                    .map_err(|_| AppError::Internal)?;
                state::delete(
                    &tx,
                    &meta.id,
                    &key,
                    scope,
                    viewer.as_ref().map(|v| v.0.as_str()),
                )?;
                if let Some(audit_key) = audit_key {
                    audit::append_in_transaction(
                        &tx,
                        &audit_key,
                        &mutation.event_id()?,
                        mutation.context(),
                        &AuditEvent {
                            operation: "state.delete".to_owned(),
                            target_type: "artifact_state".to_owned(),
                            target_id: meta.id.0.clone(),
                            result: "success".to_owned(),
                            classification: "viewer_state".to_owned(),
                            revision: None,
                        },
                    )?;
                }
                tx.commit().map_err(|_| AppError::Internal)
            })
            .await
        })
    }
}
