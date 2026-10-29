//! Retention: verify, checkpoint, and delete one contiguous expired prefix.

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

use crate::error::AppError;

use super::AUDIT_RETENTION_DAYS;
use super::append::append_in_transaction;
use super::attribution::MutationAudit;
use super::chain::checkpoint_mac;
use super::schema::AuditEvent;
use super::verify::verify;

/// Verify before the irreversible operation, then checkpoint and delete exactly one contiguous
/// expired prefix. A later (non-expired) row stops the run even if a subsequent row is old.
pub fn prune_expired(
    conn: &mut Connection,
    key: &[u8; 32],
    cutoff: Option<&str>,
    batch_size: u64,
) -> Result<u64, AppError> {
    let cutoff = match cutoff {
        Some(value) => value.to_owned(),
        None => (time::OffsetDateTime::now_utc() - time::Duration::days(AUDIT_RETENTION_DAYS))
            .format(&time::format_description::well_known::Rfc3339)
            .map_err(|_| AppError::Internal)?,
    };
    if !verify(conn, key)? {
        crate::observability::record_global_security_signal("integrity_failure");
        return Err(AppError::Validation(
            "audit ledger integrity verification failed before retention".to_owned(),
        ));
    }
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|_| AppError::Internal)?;
    let batch = batch_size.clamp(1, 1_000);
    let cutoff_julian: Option<f64> = tx
        .query_row("SELECT julianday(?1)", [&cutoff], |row| row.get(0))
        .map_err(|_| AppError::Internal)?;
    let cutoff_julian = cutoff_julian
        .ok_or_else(|| AppError::Validation("invalid audit retention cutoff".to_owned()))?;
    let prefix: Vec<(u64, String, Option<f64>)> = {
        let mut statement = tx.prepare("SELECT sequence,event_hash,julianday(occurred_at) FROM security_audit_events ORDER BY sequence ASC LIMIT ?1").map_err(|_| AppError::Internal)?;
        statement
            .query_map(
                [i64::try_from(batch).map_err(|_| AppError::Internal)?],
                |row| {
                    Ok((
                        u64::try_from(row.get::<_, i64>(0)?).unwrap_or(0),
                        row.get(1)?,
                        row.get(2)?,
                    ))
                },
            )
            .map_err(|_| AppError::Internal)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| AppError::Internal)?
    };
    let count = prefix
        .iter()
        .take_while(|(_, _, occurred_at)| occurred_at.is_some_and(|value| value < cutoff_julian))
        .count();
    if count == 0 {
        tx.commit().map_err(|_| AppError::Internal)?;
        return Ok(0);
    }
    let (first, _, _) = prefix.first().ok_or(AppError::Internal)?;
    let (last, bridge, _) = prefix.get(count - 1).ok_or(AppError::Internal)?;
    let previous: String = tx.query_row("SELECT checkpoint_hash FROM security_audit_checkpoints ORDER BY checkpoint_id DESC LIMIT 1", [], |row| row.get(0)).optional().map_err(|_| AppError::Internal)?.unwrap_or_default();
    let checkpoint = checkpoint_mac(key, *first, *last, bridge, &previous);
    tx.execute("INSERT INTO security_audit_checkpoints (first_sequence,last_sequence,key_id,canonical_version,bridge_hash,prev_checkpoint_hash,checkpoint_hash) VALUES (?1,?2,'v1',1,?3,?4,?5)", params![i64::try_from(*first).map_err(|_| AppError::Internal)?, i64::try_from(*last).map_err(|_| AppError::Internal)?, bridge, previous, checkpoint]).map_err(|_| AppError::Internal)?;
    let deleted = tx
        .execute(
            "DELETE FROM security_audit_events WHERE sequence >= ?1 AND sequence <= ?2",
            params![
                i64::try_from(*first).map_err(|_| AppError::Internal)?,
                i64::try_from(*last).map_err(|_| AppError::Internal)?
            ],
        )
        .map_err(|_| AppError::Internal)?;
    let prune_audit = MutationAudit::recovery()?;
    let mut context = prune_audit.context().clone();
    context.source = "maintenance".to_owned();
    let _ = append_in_transaction(
        &tx,
        key,
        &prune_audit.event_id()?,
        &context,
        &AuditEvent {
            operation: "audit.prune".to_owned(),
            target_type: "audit_ledger".to_owned(),
            target_id: "retention".to_owned(),
            result: "success".to_owned(),
            classification: "retention".to_owned(),
            revision: None,
        },
    )?;
    tx.commit().map_err(|_| AppError::Internal)?;
    u64::try_from(deleted).map_err(|_| AppError::Internal)
}
