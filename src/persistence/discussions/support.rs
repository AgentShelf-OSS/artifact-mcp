//! Small helpers shared by every discussion store module.

use rusqlite::Transaction;

use super::internal;
use crate::{
    error::AppError,
    security::audit::{AuditEvent, MutationAudit, append_in_transaction},
};

pub(super) fn append_audit_then_commit<T>(
    tx: Transaction<'_>,
    audit_key: &[u8; 32],
    audit: &MutationAudit,
    event: AuditEvent,
    value: T,
) -> Result<T, AppError> {
    append_in_transaction(&tx, audit_key, &audit.event_id()?, audit.context(), &event)?;
    tx.commit().map_err(internal)?;
    Ok(value)
}

pub(super) fn require_nonempty(values: &[&str]) -> Result<(), AppError> {
    if values
        .iter()
        .any(|value| value.is_empty() || value.contains('\0'))
    {
        Err(AppError::Validation(
            "discussion identity fields are required".to_owned(),
        ))
    } else {
        Ok(())
    }
}

pub(super) fn trimmed(value: &str) -> String {
    value.trim().to_owned()
}
