//! Full-ledger verification: checkpoints, then the retained event chain, then the head.

use rusqlite::Connection;

use crate::error::AppError;

use super::chain::{checkpoint_mac, hash, head_mac, load_head, pending_receipts_root};
use super::encoding::canonical;
use super::schema::{AuditContext, AuditEvent};
use super::{KEY_ID, VERSION};

/// Verify every retained event against its duplicated fields and atomic chain head.
pub fn verify(conn: &Connection, key: &[u8; 32]) -> Result<bool, AppError> {
    let mut previous_checkpoint = String::new();
    let mut expected_sequence = 1_u64;
    let mut bridge = String::new();
    let mut checkpoints = conn.prepare("SELECT first_sequence,last_sequence,key_id,canonical_version,bridge_hash,prev_checkpoint_hash,checkpoint_hash FROM security_audit_checkpoints ORDER BY checkpoint_id").map_err(|_| AppError::Internal)?;
    let checkpoint_rows = checkpoints
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, String>(6)?,
            ))
        })
        .map_err(|_| AppError::Internal)?;
    for checkpoint in checkpoint_rows {
        let (first, last, key_id, version, checkpoint_bridge, prior, stored) =
            checkpoint.map_err(|_| AppError::Internal)?;
        let (first, last) = (
            u64::try_from(first).map_err(|_| AppError::Internal)?,
            u64::try_from(last).map_err(|_| AppError::Internal)?,
        );
        if first != expected_sequence
            || last < first
            || key_id != KEY_ID
            || version != i64::from(VERSION)
            || prior != previous_checkpoint
            || stored != checkpoint_mac(key, first, last, &checkpoint_bridge, &previous_checkpoint)
        {
            return Ok(false);
        }
        expected_sequence = last.checked_add(1).ok_or(AppError::Internal)?;
        bridge = checkpoint_bridge;
        previous_checkpoint = stored;
    }
    drop(checkpoints);
    let mut previous = bridge;
    let mut statement = conn.prepare("SELECT sequence,event_id,key_id,canonical_version,tenant,actor_type,actor_id,actor_role,operation,target_type,target_id,result,classification,source,request_id,revision,occurred_at,canonical,prev_hash,event_hash FROM security_audit_events ORDER BY sequence").map_err(|_| AppError::Internal)?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
                AuditContext {
                    tenant: row.get(4)?,
                    actor_type: row.get(5)?,
                    actor_id: row.get(6)?,
                    actor_role: row.get(7)?,
                    source: row.get(13)?,
                    request_id: row.get(14)?,
                },
                AuditEvent {
                    operation: row.get(8)?,
                    target_type: row.get(9)?,
                    target_id: row.get(10)?,
                    result: row.get(11)?,
                    classification: row.get(12)?,
                    revision: row
                        .get::<_, Option<i64>>(15)?
                        .map(|v| u64::try_from(v).unwrap_or(u64::MAX)),
                },
                row.get::<_, String>(16)?,
                row.get::<_, Vec<u8>>(17)?,
                row.get::<_, String>(18)?,
                row.get::<_, String>(19)?,
            ))
        })
        .map_err(|_| AppError::Internal)?;
    for item in rows {
        let (
            sequence,
            event_id,
            key_id,
            version,
            context,
            event,
            occurred_at,
            stored,
            prev,
            stored_hash,
        ) = item.map_err(|_| AppError::Internal)?;
        let sequence = u64::try_from(sequence).map_err(|_| AppError::Internal)?;
        if sequence != expected_sequence
            || key_id != KEY_ID
            || version != i64::from(VERSION)
            || prev != previous
            || canonical(&event_id, &context, &event, &occurred_at)? != stored
            || hash(key, sequence, &previous, &stored) != stored_hash
        {
            return Ok(false);
        }
        previous = stored_hash;
        expected_sequence += 1;
    }
    let head = load_head(conn)?;
    let recomputed_pending_root = pending_receipts_root(conn)?;
    Ok(
        u64::try_from(head.sequence).ok() == Some(expected_sequence - 1)
            && head.key_id == KEY_ID
            && head.canonical_version == i64::from(VERSION)
            && head.head_hash == previous
            && head.pending_receipts_root == recomputed_pending_root
            && head.head_mac
                == head_mac(
                    key,
                    expected_sequence - 1,
                    &previous,
                    &recomputed_pending_root,
                ),
    )
}
