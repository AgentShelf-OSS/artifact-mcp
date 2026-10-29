//! The ledger writer: appends one terminal event inside the caller's business transaction.

use rusqlite::{Connection, Transaction, TransactionBehavior, params};

use crate::error::AppError;

use super::attribution::MutationAudit;
use super::chain::{hash, head_mac, initialize_head, load_head, verify_head_mac};
use super::encoding::canonical;
use super::receipts::verify_pending_receipts;
use super::schema::{AuditContext, AuditEvent, AuditRow};

/// Append atomically with the audited business transaction. The caller owns `tx`; a failing
/// ledger write aborts that transaction and therefore cannot fabricate mutation success.
pub fn append_in_transaction(
    tx: &Transaction<'_>,
    key: &[u8; 32],
    event_id: &str,
    context: &AuditContext,
    event: &AuditEvent,
) -> Result<AuditRow, AppError> {
    // Migrations create an unsealed empty head. Seal it in the first mutation transaction so
    // no runtime can append to an unauthenticated head.
    initialize_head(tx, key)?;
    verify_pending_receipts(tx, key)?;
    let old = load_head(tx)?;
    let old_sequence = verify_head_mac(key, &old)?;
    let sequence = old_sequence.checked_add(1).ok_or(AppError::Internal)?;
    // Timestamps are generated at the persistence boundary; any value supplied in the event
    // struct is deliberately ignored so a route cannot backdate a terminal ledger record.
    let occurred_at = time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .map_err(|_| AppError::Internal)?;
    let persisted_event = event.clone();
    let bytes = canonical(event_id, context, &persisted_event, &occurred_at)?;
    let event_hash = hash(key, sequence, &old.head_hash, &bytes);
    let revision = persisted_event
        .revision
        .map(i64::try_from)
        .transpose()
        .map_err(|_| AppError::Internal)?;
    tx.execute("INSERT INTO security_audit_events (sequence,event_id,key_id,tenant,actor_type,actor_id,actor_role,operation,target_type,target_id,result,classification,source,request_id,revision,occurred_at,canonical_version,canonical,prev_hash,event_hash) VALUES (?1,?2,'v1',?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,1,?16,?17,?18)", params![i64::try_from(sequence).map_err(|_| AppError::Internal)?, event_id, context.tenant, context.actor_type, context.actor_id, context.actor_role, persisted_event.operation, persisted_event.target_type, persisted_event.target_id, persisted_event.result, persisted_event.classification, context.source, context.request_id, revision, occurred_at, bytes, old.head_hash, event_hash]).map_err(|_| AppError::Internal)?;
    let new_mac = head_mac(key, sequence, &event_hash, &old.pending_receipts_root);
    if tx.execute("UPDATE security_audit_chain_head SET sequence=?1,key_id='v1',head_hash=?2,head_mac=?3,updated_at=datetime('now') WHERE singleton=1 AND sequence=?4 AND head_hash=?5 AND head_mac=?6 AND pending_receipts_root=?7", params![i64::try_from(sequence).map_err(|_| AppError::Internal)?, event_hash, new_mac, old.sequence, old.head_hash, old.head_mac, old.pending_receipts_root]).map_err(|_| AppError::Internal)? != 1 { return Err(AppError::Internal); }
    Ok(AuditRow {
        sequence,
        event_id: event_id.to_owned(),
        tenant: context.tenant.clone(),
        operation: persisted_event.operation.clone(),
        target_type: persisted_event.target_type.clone(),
        target_id: persisted_event.target_id.clone(),
        result: persisted_event.result.clone(),
        classification: persisted_event.classification.clone(),
        occurred_at,
        event_hash,
    })
}

/// Execute a business mutation and its terminal ledger record in one SQLite transaction.
///
/// The event is constructed only after the mutation has produced its server-derived target.
/// Dropping the transaction on either error is intentional: a caller must never report a
/// privileged mutation that cannot be represented in the tamper-evident ledger.
pub fn mutate_in_transaction<T>(
    conn: &mut Connection,
    key: &[u8; 32],
    audit: &MutationAudit,
    mutation: impl FnOnce(&Transaction<'_>) -> Result<(T, AuditEvent), AppError>,
) -> Result<T, AppError> {
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|_| AppError::Internal)?;
    let (value, event) = mutation(&tx)?;
    append_in_transaction(&tx, key, &audit.event_id()?, audit.context(), &event)?;
    tx.commit().map_err(|_| AppError::Internal)?;
    Ok(value)
}
