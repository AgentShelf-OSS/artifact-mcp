//! The runtime-owned access service that holds the pool and the validated key.

use crate::error::AppError;
use crate::model::PublisherIdentity;
use crate::persistence::db::{self, DbPool};

use super::append::mutate_in_transaction;
use super::attribution::MutationAudit;
use super::query::{
    AuditExport, AuditExportQuery, AuditPage, AuditQuery, export_authorized, query_authorized,
};
use super::retention::prune_expired;
use super::schema::AuditEvent;

/// One runtime-owned access service. It intentionally owns only a pool and the already-validated
/// HMAC key; it never carries an actor credential or exposes the key in Debug/display.
#[derive(Clone)]
pub struct AuditAccess {
    pool: DbPool,
    key: [u8; 32],
}

impl std::fmt::Debug for AuditAccess {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("AuditAccess(<redacted>)")
    }
}

impl AuditAccess {
    #[must_use]
    pub const fn new(pool: DbPool, key: [u8; 32]) -> Self {
        Self { pool, key }
    }

    /// Crate-local copy used by persistence adapters that must append an audit receipt in the
    /// same SQLite transaction as their business mutation. The key never crosses the crate
    /// boundary or appears in diagnostics.
    pub(crate) const fn mutation_key(&self) -> [u8; 32] {
        self.key
    }

    /// Record the fixed external read-check action before or after dispatch.
    pub async fn record_live_action(
        &self,
        audit: MutationAudit,
        artifact_id: String,
        revision: u64,
        result: &str,
        classification: &str,
    ) -> Result<(), AppError> {
        let pool = self.pool.clone();
        let key = self.key;
        let result = result.to_owned();
        let classification = classification.to_owned();
        let action_operation = if classification.starts_with("investigate-finding.") {
            "artifact.action.investigate-finding"
        } else if classification.starts_with("analyze-differences.") {
            "artifact.action.analyze-differences"
        } else {
            "artifact.action.check-live-signals"
        };
        db::interact(&pool, move |conn| {
            mutate_in_transaction(conn, &key, &audit, |_| {
                Ok((
                    (),
                    AuditEvent {
                        operation: action_operation.into(),
                        target_type: "artifact".into(),
                        target_id: artifact_id,
                        result,
                        classification,
                        revision: Some(revision),
                    },
                ))
            })
        })
        .await
    }

    pub async fn query(
        &self,
        actor: &PublisherIdentity,
        query: AuditQuery,
    ) -> Result<AuditPage, AppError> {
        let pool = self.pool.clone();
        let key = self.key;
        let actor = actor.clone();
        db::interact(&pool, move |conn| {
            query_authorized(conn, &key, &actor, query)
        })
        .await
    }

    pub async fn export(
        &self,
        actor: &PublisherIdentity,
        query: AuditExportQuery,
    ) -> Result<AuditExport, AppError> {
        let pool = self.pool.clone();
        let key = self.key;
        let actor = actor.clone();
        db::interact(&pool, move |conn| {
            export_authorized(conn, &key, &actor, query)
        })
        .await
    }

    /// Retention is deliberately an operator-only API, not an MCP tool. Its caller must arrange
    /// scheduling outside this process; every execution verifies the complete current ledger.
    pub async fn prune_expired(&self) -> Result<u64, AppError> {
        let pool = self.pool.clone();
        let key = self.key;
        db::interact(&pool, move |conn| prune_expired(conn, &key, None, 1_000)).await
    }
}
