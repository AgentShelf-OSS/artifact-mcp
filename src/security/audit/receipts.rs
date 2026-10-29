//! Durable cross-resource receipts: reservation, finalization, reconciliation, and release,
//! each authenticated against the pending-set commitment in the chain head.

use hmac::Mac;
use rusqlite::{Connection, OptionalExtension, Transaction, params};

use crate::error::AppError;

use super::append::append_in_transaction;
use super::chain::{
    head_mac, initialize_head, load_head, pending_receipts_root, receipt_mac, verify_head_mac,
};
use super::encoding::{ReceiptMacInput, canonical, receipt_message};
use super::schema::{AuditContext, AuditEvent, AuditRow};
use super::{HmacSha256, KEY_ID, VERSION};

#[derive(Debug)]
struct PersistedReceipt {
    event_id: Option<String>,
    state: String,
    correlation_id: String,
    durability_intent_id: String,
    tenant: String,
    actor_type: String,
    actor_id: String,
    actor_role: String,
    source: String,
    request_id: String,
    operation: String,
    target_type: String,
    target_id: String,
    result: String,
    classification: String,
    revision: Option<i64>,
    key_id: String,
    canonical_version: i64,
    receipt_mac: String,
}

struct PersistedEventProjection {
    event_id: String,
    key_id: String,
    tenant: String,
    actor_type: String,
    actor_id: String,
    actor_role: String,
    operation: String,
    target_type: String,
    target_id: String,
    result: String,
    classification: String,
    source: String,
    request_id: String,
    revision: Option<i64>,
    canonical_version: i64,
}

/// Verify the authenticated commitment before reconciliation or a receipt transition. A missing
/// pending row changes the recomputed root even though receipt lookup itself returns no row.
pub fn verify_pending_receipts(conn: &Connection, key: &[u8; 32]) -> Result<(), AppError> {
    let head = load_head(conn)?;
    let _ = verify_head_mac(key, &head)?;
    if head.pending_receipts_root != pending_receipts_root(conn)? {
        return Err(AppError::Internal);
    }
    let mut statement = conn
        .prepare(&format!(
            "{RECEIPT_SELECT} WHERE state='pending' ORDER BY correlation_id ASC"
        ))
        .map_err(|_| AppError::Internal)?;
    let receipts = statement
        .query_map([], receipt_from_row)
        .map_err(|_| AppError::Internal)?;
    for receipt in receipts {
        verify_receipt_snapshot(key, &receipt.map_err(|_| AppError::Internal)?)?;
    }
    Ok(())
}

fn refresh_pending_receipts_root(tx: &Transaction<'_>, key: &[u8; 32]) -> Result<(), AppError> {
    let head = load_head(tx)?;
    let sequence = verify_head_mac(key, &head)?;
    let new_root = pending_receipts_root(tx)?;
    let new_mac = head_mac(key, sequence, &head.head_hash, &new_root);
    if tx
        .execute(
            "UPDATE security_audit_chain_head \
             SET pending_receipts_root=?1,head_mac=?2,updated_at=datetime('now') \
             WHERE singleton=1 AND sequence=?3 AND head_hash=?4 AND head_mac=?5 \
               AND pending_receipts_root=?6",
            params![
                new_root,
                new_mac,
                head.sequence,
                head.head_hash,
                head.head_mac,
                head.pending_receipts_root
            ],
        )
        .map_err(|_| AppError::Internal)?
        != 1
    {
        return Err(AppError::Internal);
    }
    Ok(())
}

impl PersistedReceipt {
    fn context(&self) -> AuditContext {
        AuditContext {
            tenant: self.tenant.clone(),
            actor_type: self.actor_type.clone(),
            actor_id: self.actor_id.clone(),
            actor_role: self.actor_role.clone(),
            source: self.source.clone(),
            request_id: self.request_id.clone(),
        }
    }

    fn event(&self) -> Result<AuditEvent, AppError> {
        Ok(AuditEvent {
            operation: self.operation.clone(),
            target_type: self.target_type.clone(),
            target_id: self.target_id.clone(),
            result: self.result.clone(),
            classification: self.classification.clone(),
            revision: self
                .revision
                .map(u64::try_from)
                .transpose()
                .map_err(|_| AppError::Internal)?,
        })
    }
}

fn receipt_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<PersistedReceipt> {
    Ok(PersistedReceipt {
        event_id: row.get(0)?,
        state: row.get(1)?,
        correlation_id: row.get(2)?,
        durability_intent_id: row.get(3)?,
        tenant: row.get(4)?,
        actor_type: row.get(5)?,
        actor_id: row.get(6)?,
        actor_role: row.get(7)?,
        source: row.get(8)?,
        request_id: row.get(9)?,
        operation: row.get(10)?,
        target_type: row.get(11)?,
        target_id: row.get(12)?,
        result: row.get(13)?,
        classification: row.get(14)?,
        revision: row.get(15)?,
        key_id: row.get(16)?,
        canonical_version: row.get(17)?,
        receipt_mac: row.get(18)?,
    })
}

const RECEIPT_SELECT: &str = "SELECT event_id,state,correlation_id,durability_intent_id,tenant,actor_type,actor_id,actor_role,source,request_id,operation,target_type,target_id,result,classification,revision,key_id,canonical_version,receipt_mac FROM security_audit_receipts";

fn verify_receipt_snapshot(key: &[u8; 32], receipt: &PersistedReceipt) -> Result<(), AppError> {
    if receipt.key_id != KEY_ID || receipt.canonical_version != i64::from(VERSION) {
        return Err(AppError::Internal);
    }
    let context = receipt.context();
    let event = receipt.event()?;
    let message = receipt_message(ReceiptMacInput {
        correlation_id: &receipt.correlation_id,
        durability_intent_id: &receipt.durability_intent_id,
        state: &receipt.state,
        event_id: receipt.event_id.as_deref().unwrap_or_default(),
        context: &context,
        event: &event,
        key_id: &receipt.key_id,
        canonical_version: receipt.canonical_version,
    })
    .map_err(|_| AppError::Internal)?;
    let supplied = hex::decode(&receipt.receipt_mac).map_err(|_| AppError::Internal)?;
    if supplied.len() != 32 {
        return Err(AppError::Internal);
    }
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts 32-byte key");
    mac.update(&message);
    mac.verify_slice(&supplied).map_err(|_| AppError::Internal)
}

fn verify_finalized_event_projection(
    tx: &Transaction<'_>,
    receipt: &PersistedReceipt,
) -> Result<(), AppError> {
    let event_id = receipt.event_id.as_deref().ok_or(AppError::Internal)?;
    let persisted: PersistedEventProjection = tx
        .query_row(
            "SELECT event_id,key_id,tenant,actor_type,actor_id,actor_role,operation,target_type,\
                    target_id,result,classification,source,request_id,revision,canonical_version \
             FROM security_audit_events WHERE event_id=?1",
            [event_id],
            |row| {
                Ok(PersistedEventProjection {
                    event_id: row.get(0)?,
                    key_id: row.get(1)?,
                    tenant: row.get(2)?,
                    actor_type: row.get(3)?,
                    actor_id: row.get(4)?,
                    actor_role: row.get(5)?,
                    operation: row.get(6)?,
                    target_type: row.get(7)?,
                    target_id: row.get(8)?,
                    result: row.get(9)?,
                    classification: row.get(10)?,
                    source: row.get(11)?,
                    request_id: row.get(12)?,
                    revision: row.get(13)?,
                    canonical_version: row.get(14)?,
                })
            },
        )
        .map_err(|_| AppError::Internal)?;
    if persisted.event_id != event_id
        || persisted.key_id != receipt.key_id
        || persisted.tenant != receipt.tenant
        || persisted.actor_type != receipt.actor_type
        || persisted.actor_id != receipt.actor_id
        || persisted.actor_role != receipt.actor_role
        || persisted.operation != receipt.operation
        || persisted.target_type != receipt.target_type
        || persisted.target_id != receipt.target_id
        || persisted.result != receipt.result
        || persisted.classification != receipt.classification
        || persisted.source != receipt.source
        || persisted.request_id != receipt.request_id
        || persisted.revision != receipt.revision
        || persisted.canonical_version != receipt.canonical_version
    {
        return Err(AppError::Internal);
    }
    Ok(())
}

fn receipt_by_correlation(
    tx: &Transaction<'_>,
    correlation_id: &str,
) -> Result<Option<PersistedReceipt>, AppError> {
    tx.query_row(
        &format!("{RECEIPT_SELECT} WHERE correlation_id=?1"),
        [correlation_id],
        receipt_from_row,
    )
    .optional()
    .map_err(|_| AppError::Internal)
}

fn receipt_by_intent(
    tx: &Transaction<'_>,
    intent_id: &str,
) -> Result<Option<PersistedReceipt>, AppError> {
    tx.query_row(
        &format!("{RECEIPT_SELECT} WHERE durability_intent_id=?1"),
        [intent_id],
        receipt_from_row,
    )
    .optional()
    .map_err(|_| AppError::Internal)
}

/// Reserve a PBI-051 receipt in the exact same metadata transaction as its durability intent.
pub fn reserve_receipt_in_transaction(
    tx: &Transaction<'_>,
    key: &[u8; 32],
    correlation: &str,
    intent: &str,
    context: &AuditContext,
    event: &AuditEvent,
) -> Result<bool, AppError> {
    initialize_head(tx, key)?;
    verify_pending_receipts(tx, key)?;
    // Use the exact event canonicalizer as the receipt admission gate. A receipt is durable
    // audit storage too; it must never become a side channel for token/body persistence.
    let _ = canonical("receipt", context, event, "")?;
    let revision = event
        .revision
        .map(i64::try_from)
        .transpose()
        .map_err(|_| AppError::Internal)?;
    let receipt_mac = receipt_mac(key, correlation, intent, "pending", "", context, event)?;
    // A pending receipt is evidence of an unresolved cross-resource mutation.  Retrying must
    // never rewrite its actor, target, or correlation: an ambiguous process death is resolved
    // only by reconciliation, which either finalizes the immutable snapshot or atomically
    // proves/release it.  In-process compensation deletes the receipt with its intent, so a
    // clean retry still inserts a fresh reservation.
    let changes = tx.execute("INSERT INTO security_audit_receipts (correlation_id,durability_intent_id,state,operation,target_type,target_id,result,tenant,actor_type,actor_id,actor_role,source,request_id,revision,classification,key_id,canonical_version,receipt_mac) VALUES (?1,?2,'pending',?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,'v1',1,?15) ON CONFLICT(durability_intent_id) DO NOTHING", params![correlation, intent, event.operation, event.target_type, event.target_id, event.result, context.tenant, context.actor_type, context.actor_id, context.actor_role, context.source, context.request_id, revision, event.classification, receipt_mac]).map_err(|_| AppError::Internal)?;
    if changes == 1 {
        refresh_pending_receipts_root(tx, key)?;
    }
    Ok(changes == 1)
}

/// Finalize a reserved receipt exactly once, appending the only terminal event.
pub fn finalize_receipt_in_transaction(
    tx: &Transaction<'_>,
    key: &[u8; 32],
    correlation: &str,
    event_id: &str,
    context: &AuditContext,
    event: &AuditEvent,
) -> Result<(AuditRow, bool), AppError> {
    verify_pending_receipts(tx, key)?;
    let Some(existing) = receipt_by_correlation(tx, correlation)? else {
        return Err(AppError::Internal);
    };
    // Authenticate the durable snapshot before using any persisted field, including on the
    // finalized duplicate path. An attacker who can edit SQLite still cannot relabel an intent
    // and have this process bless the forged projection into a new valid chain entry.
    verify_receipt_snapshot(key, &existing)?;
    // Validate the supplied retry against the persisted server-derived snapshot before the
    // finalized fast path. Correlation ids are not authority to relabel an old mutation.
    let _ = canonical("receipt", context, event, "")?;
    let expected_revision = event
        .revision
        .map(i64::try_from)
        .transpose()
        .map_err(|_| AppError::Internal)?;
    if existing.tenant != context.tenant
        || existing.actor_type != context.actor_type
        || existing.actor_id != context.actor_id
        || existing.actor_role != context.actor_role
        || existing.source != context.source
        || existing.request_id != context.request_id
        || existing.operation != event.operation
        || existing.target_type != event.target_type
        || existing.target_id != event.target_id
        || existing.result != event.result
        || existing.classification != event.classification
        || existing.revision != expected_revision
    {
        return Err(AppError::Validation(
            "audit receipt context mismatch".to_owned(),
        ));
    }
    if existing.state == "finalized" {
        verify_finalized_event_projection(tx, &existing)?;
        let id = existing.event_id.ok_or(AppError::Internal)?;
        let row = tx.query_row("SELECT sequence,event_id,tenant,operation,target_type,target_id,result,classification,occurred_at,event_hash FROM security_audit_events WHERE event_id=?1", [id], |row| Ok(AuditRow { sequence: u64::try_from(row.get::<_, i64>(0)?).unwrap_or(0), event_id: row.get(1)?, tenant: row.get(2)?, operation: row.get(3)?, target_type: row.get(4)?, target_id: row.get(5)?, result: row.get(6)?, classification: row.get(7)?, occurred_at: row.get(8)?, event_hash: row.get(9)? })).map_err(|_| AppError::Internal)?;
        return Ok((row, true));
    }
    if existing.state != "pending" {
        return Err(AppError::Internal);
    }
    let row = append_in_transaction(tx, key, event_id, context, event)?;
    let finalized_mac = receipt_mac(
        key,
        &existing.correlation_id,
        &existing.durability_intent_id,
        "finalized",
        event_id,
        context,
        event,
    )?;
    if tx.execute("UPDATE security_audit_receipts SET event_id=?1,state='finalized',receipt_mac=?2,finalized_at=datetime('now') WHERE correlation_id=?3 AND state='pending' AND receipt_mac=?4", params![event_id, finalized_mac, correlation, existing.receipt_mac]).map_err(|_| AppError::Internal)? != 1 { return Err(AppError::Internal); }
    refresh_pending_receipts_root(tx, key)?;
    Ok((row, false))
}

/// Reconciliation owns the only transition for a pending cross-resource receipt after a restart.
/// It reconstructs actor/target fields from the immutable reservation rather than accepting a
/// new caller context, and appends one terminal recovered/failure event before marking it final.
/// Legacy durability intents pre-date receipts and return `false` without blocking recovery.
pub fn finalize_reconciled_receipt_in_transaction(
    tx: &Transaction<'_>,
    key: &[u8; 32],
    intent_id: &str,
    event_id: &str,
    result: &str,
    classification: &str,
) -> Result<bool, AppError> {
    verify_pending_receipts(tx, key)?;
    let Some(receipt) = receipt_by_intent(tx, intent_id)? else {
        return Ok(false);
    };
    if receipt.state != "pending" || !matches!(result, "recovered" | "failure") {
        return Err(AppError::Internal);
    }
    verify_receipt_snapshot(key, &receipt)?;
    let context = receipt.context();
    let mut event = receipt.event()?;
    event.result = result.to_owned();
    event.classification = classification.to_owned();
    append_in_transaction(tx, key, event_id, &context, &event)?;
    // A proven rollback/failure releases the deterministic intent identifier for a later fresh
    // request.  Its immutable terminal event remains in the ledger; retaining a finalized
    // receipt would make the next retry collide on `durability_intent_id`.  Recovered success
    // remains finalized for exactly-once retry reads.
    let changes = if result == "failure" {
        tx.execute(
            "DELETE FROM security_audit_receipts \
             WHERE correlation_id=?1 AND state='pending' AND receipt_mac=?2",
            params![receipt.correlation_id, receipt.receipt_mac],
        )
    } else {
        let finalized_mac = receipt_mac(
            key,
            &receipt.correlation_id,
            &receipt.durability_intent_id,
            "finalized",
            event_id,
            &context,
            &event,
        )?;
        tx.execute(
            "UPDATE security_audit_receipts \
             SET event_id=?1,state='finalized',result=?2,classification=?3,receipt_mac=?4,\
                 finalized_at=datetime('now') \
             WHERE correlation_id=?5 AND state='pending' AND receipt_mac=?6",
            params![
                event_id,
                result,
                classification,
                finalized_mac,
                receipt.correlation_id,
                receipt.receipt_mac
            ],
        )
    }
    .map_err(|_| AppError::Internal)?;
    if changes != 1 {
        return Err(AppError::Internal);
    }
    refresh_pending_receipts_root(tx, key)?;
    Ok(true)
}

/// Authenticate a pending receipt before an in-process compensation releases it. The caller
/// keeps this verification, the receipt deletion, the durability-intent deletion, and the
/// terminal failure append in one transaction.
pub fn verify_pending_receipt_in_transaction(
    tx: &Transaction<'_>,
    key: &[u8; 32],
    intent_id: &str,
) -> Result<(), AppError> {
    verify_pending_receipts(tx, key)?;
    let receipt = receipt_by_intent(tx, intent_id)?.ok_or(AppError::Internal)?;
    if receipt.state != "pending" {
        return Err(AppError::Internal);
    }
    verify_receipt_snapshot(key, &receipt)
}

/// Release an authenticated pending receipt after its terminal compensation event has been
/// appended in the same transaction, then advance the head's pending-set commitment.
pub fn delete_pending_receipt_in_transaction(
    tx: &Transaction<'_>,
    key: &[u8; 32],
    intent_id: &str,
) -> Result<(), AppError> {
    verify_pending_receipt_in_transaction(tx, key, intent_id)?;
    let receipt = receipt_by_intent(tx, intent_id)?.ok_or(AppError::Internal)?;
    if tx
        .execute(
            "DELETE FROM security_audit_receipts \
             WHERE durability_intent_id=?1 AND state='pending' AND receipt_mac=?2",
            params![intent_id, receipt.receipt_mac],
        )
        .map_err(|_| AppError::Internal)?
        != 1
    {
        return Err(AppError::Internal);
    }
    refresh_pending_receipts_root(tx, key)
}
