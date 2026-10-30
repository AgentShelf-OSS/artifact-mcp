//! The HMAC key, the event hash chain, and the authenticated chain head and checkpoints.

use hmac::{KeyInit as _, Mac};
use rusqlite::{Connection, params};
use sha2::{Digest as _, Sha256};

use crate::error::AppError;

use super::encoding::{ReceiptMacInput, receipt_message};
use super::receipts::verify_pending_receipts;
use super::schema::{AuditContext, AuditEvent};
use super::{DOMAIN, HmacSha256, KEY_ID, VERSION};

#[derive(Debug)]
pub(super) struct PersistedHead {
    pub(super) sequence: i64,
    pub(super) key_id: String,
    pub(super) head_hash: String,
    pub(super) head_mac: String,
    pub(super) canonical_version: i64,
    pub(super) pending_receipts_root: String,
}

/// Strict canonical base64 audit key parser; it is deliberately separate from optional config
/// parsing so deterministic tests can inject a fixture key without process environment state.
pub fn parse_hmac_key(value: &str) -> Result<[u8; 32], AppError> {
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    let decoded = STANDARD.decode(value.trim()).map_err(|_| {
        AppError::Validation(
            "AUDIT_LEDGER_HMAC_KEY must be canonical base64 encoding of exactly 32 bytes."
                .to_owned(),
        )
    })?;
    if decoded.len() != 32 || STANDARD.encode(&decoded) != value.trim() {
        return Err(AppError::Validation(
            "AUDIT_LEDGER_HMAC_KEY must be canonical base64 encoding of exactly 32 bytes."
                .to_owned(),
        ));
    }
    decoded.try_into().map_err(|_| AppError::Internal)
}

pub(super) fn receipt_mac(
    key: &[u8; 32],
    correlation_id: &str,
    durability_intent_id: &str,
    state: &str,
    event_id: &str,
    context: &AuditContext,
    event: &AuditEvent,
) -> Result<String, AppError> {
    let message = receipt_message(ReceiptMacInput {
        correlation_id,
        durability_intent_id,
        state,
        event_id,
        context,
        event,
        key_id: KEY_ID,
        canonical_version: i64::from(VERSION),
    })?;
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts 32-byte key");
    mac.update(&message);
    Ok(hex::encode(mac.finalize().into_bytes()))
}

fn part(out: &mut Vec<u8>, value: &[u8]) {
    out.extend_from_slice(&(u32::try_from(value.len()).unwrap_or(u32::MAX)).to_be_bytes());
    out.extend_from_slice(value);
}
pub(super) fn hash(key: &[u8; 32], sequence: u64, previous: &str, canonical: &[u8]) -> String {
    let mut message = Vec::new();
    message.extend_from_slice(DOMAIN);
    message.extend_from_slice(&sequence.to_be_bytes());
    part(&mut message, previous.as_bytes());
    part(&mut message, canonical);
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts 32-byte key");
    mac.update(&message);
    hex::encode(mac.finalize().into_bytes())
}

pub(super) fn pending_receipts_root(conn: &Connection) -> Result<String, AppError> {
    let mut statement = conn
        .prepare(
            "SELECT receipt_mac FROM security_audit_receipts \
             WHERE state='pending' ORDER BY receipt_mac ASC",
        )
        .map_err(|_| AppError::Internal)?;
    let receipts = statement
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|_| AppError::Internal)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|_| AppError::Internal)?;
    let mut message = Vec::new();
    message.extend_from_slice(DOMAIN);
    message.extend_from_slice(b"pending-receipts\0");
    message.extend_from_slice(
        &u64::try_from(receipts.len())
            .map_err(|_| AppError::Internal)?
            .to_be_bytes(),
    );
    for receipt_mac in receipts {
        part(&mut message, receipt_mac.as_bytes());
    }
    Ok(hex::encode(Sha256::digest(&message)))
}

pub(super) fn head_mac(key: &[u8; 32], sequence: u64, head: &str, pending_root: &str) -> String {
    let mut message = Vec::new();
    message.extend_from_slice(DOMAIN);
    message.extend_from_slice(b"head\0");
    part(&mut message, KEY_ID.as_bytes());
    message.push(VERSION);
    message.extend_from_slice(&sequence.to_be_bytes());
    part(&mut message, head.as_bytes());
    part(&mut message, pending_root.as_bytes());
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts 32-byte key");
    mac.update(&message);
    hex::encode(mac.finalize().into_bytes())
}

pub(super) fn load_head(conn: &Connection) -> Result<PersistedHead, AppError> {
    conn.query_row(
        "SELECT sequence,key_id,head_hash,head_mac,canonical_version,pending_receipts_root \
         FROM security_audit_chain_head WHERE singleton=1",
        [],
        |row| {
            Ok(PersistedHead {
                sequence: row.get(0)?,
                key_id: row.get(1)?,
                head_hash: row.get(2)?,
                head_mac: row.get(3)?,
                canonical_version: row.get(4)?,
                pending_receipts_root: row.get(5)?,
            })
        },
    )
    .map_err(|_| AppError::Internal)
}

pub(super) fn verify_head_mac(key: &[u8; 32], head: &PersistedHead) -> Result<u64, AppError> {
    let sequence = u64::try_from(head.sequence).map_err(|_| AppError::Internal)?;
    if head.key_id != KEY_ID
        || head.canonical_version != i64::from(VERSION)
        || head.head_mac != head_mac(key, sequence, &head.head_hash, &head.pending_receipts_root)
    {
        return Err(AppError::Internal);
    }
    Ok(sequence)
}

pub(super) fn checkpoint_mac(
    key: &[u8; 32],
    first: u64,
    last: u64,
    bridge: &str,
    previous: &str,
) -> String {
    let mut message = Vec::new();
    message.extend_from_slice(DOMAIN);
    message.extend_from_slice(b"checkpoint\0");
    message.extend_from_slice(&first.to_be_bytes());
    message.extend_from_slice(&last.to_be_bytes());
    part(&mut message, KEY_ID.as_bytes());
    message.push(VERSION);
    part(&mut message, bridge.as_bytes());
    part(&mut message, previous.as_bytes());
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts 32-byte key");
    mac.update(&message);
    hex::encode(mac.finalize().into_bytes())
}

/// Seal the initial empty head once, before accepting mutations. An empty unsealed head is only
/// valid at migration time; a non-empty or mismatched head fails closed.
pub fn initialize_head(conn: &Connection, key: &[u8; 32]) -> Result<(), AppError> {
    let pending_root = pending_receipts_root(conn)?;
    let initial_mac = head_mac(key, 0, "", &pending_root);
    let changed = conn
        .execute(
            "UPDATE security_audit_chain_head \
         SET pending_receipts_root=?1,head_mac=?2,updated_at=datetime('now') \
         WHERE singleton=1 AND sequence=0 AND key_id='v1' AND head_hash='' AND head_mac='' \
           AND pending_receipts_root='' AND canonical_version=1",
            params![pending_root, initial_mac],
        )
        .map_err(|_| AppError::Internal)?;
    if changed > 1 {
        return Err(AppError::Internal);
    }
    verify_pending_receipts(conn, key)
}
