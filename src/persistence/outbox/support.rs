use crate::error::AppError;
use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};

use super::{DeliveryRecord, EnqueueDelivery};

pub(super) fn find_event(
    conn: &Connection,
    input: &EnqueueDelivery,
) -> Result<Option<DeliveryRecord>, AppError> {
    conn.query_row("SELECT id,event_id,tenant,event_type,target_key,bucket_id,secret_ref,payload,payload_sha256,durability_intent_id,delivery_kind,ordering_key,depends_on_outbox_id,state,attempts,next_attempt_at,lease_owner,lease_expires_at,lease_token,lease_version,result_classification,duplicate_risk,discord_message_id,terminal_error,created_at,updated_at,completed_at FROM provider_delivery_outbox WHERE provider='discord' AND tenant=?1 AND target_key=?2 AND event_id=?3", params![input.tenant,input.target_key,input.event_id], row).optional().map_err(internal)
}
pub(super) fn get(conn: &Connection, id: &str) -> Result<Option<DeliveryRecord>, AppError> {
    conn.query_row("SELECT id,event_id,tenant,event_type,target_key,bucket_id,secret_ref,payload,payload_sha256,durability_intent_id,delivery_kind,ordering_key,depends_on_outbox_id,state,attempts,next_attempt_at,lease_owner,lease_expires_at,lease_token,lease_version,result_classification,duplicate_risk,discord_message_id,terminal_error,created_at,updated_at,completed_at FROM provider_delivery_outbox WHERE id=?1", [id], row).optional().map_err(internal)
}
fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<DeliveryRecord> {
    Ok(DeliveryRecord {
        id: r.get(0)?,
        event_id: r.get(1)?,
        tenant: r.get(2)?,
        event_type: r.get(3)?,
        target_key: r.get(4)?,
        bucket_id: r.get(5)?,
        secret_ref: r.get(6)?,
        payload: r.get(7)?,
        payload_sha256: r.get(8)?,
        durability_intent_id: r.get(9)?,
        delivery_kind: r.get(10)?,
        ordering_key: r.get(11)?,
        depends_on_outbox_id: r.get(12)?,
        state: r.get(13)?,
        attempts: r.get(14)?,
        next_attempt_at: r.get(15)?,
        lease_owner: r.get(16)?,
        lease_expires_at: r.get(17)?,
        lease_token: r.get(18)?,
        lease_version: r.get(19)?,
        result_classification: r.get(20)?,
        duplicate_risk: r.get::<_, i64>(21)? != 0,
        discord_message_id: r.get(22)?,
        terminal_error: r.get(23)?,
        created_at: r.get(24)?,
        updated_at: r.get(25)?,
        completed_at: r.get(26)?,
    })
}
pub(super) fn hash(payload: &[u8]) -> String {
    hex::encode(Sha256::digest(payload))
}

pub(super) fn internal(error: rusqlite::Error) -> AppError {
    tracing::error!(error = %error, "outbox database operation failed");
    AppError::Internal
}
