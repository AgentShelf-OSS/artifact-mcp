//! Capability-gated, tenant-scoped, cursor-paged readers and the bounded export.

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use hmac::{KeyInit as _, Mac};
use rusqlite::{Connection, params};
use serde::Serialize;

use crate::error::AppError;
use crate::model::PublisherIdentity;

use super::schema::AuditRow;
use super::{
    AUDIT_DEFAULT_LIMIT, AUDIT_EXPORT_CAPABILITY, AUDIT_EXPORT_MAX_BYTES, AUDIT_EXPORT_MAX_ROWS,
    AUDIT_GLOBAL_CAPABILITY, AUDIT_MAX_LIMIT, AUDIT_READ_CAPABILITY, DOMAIN, HmacSha256,
};

/// A parsed, bounded request for a tenant-scoped ledger page. The caller identity is never
/// inferred from the request; capability admission happens before this type reaches SQLite.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AuditQuery {
    pub tenant: Option<String>,
    pub cursor: Option<String>,
    pub limit: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct AuditPage {
    pub events: Vec<AuditRow>,
    pub next: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuditExportQuery {
    pub tenant: Option<String>,
    pub cursor: Option<String>,
    pub limit: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct AuditExport {
    pub ndjson: String,
    pub rows: usize,
    pub bytes: usize,
    pub truncated: bool,
    pub next: Option<String>,
    pub reason: Option<&'static str>,
}

fn audit_error(message: &str) -> AppError {
    AppError::Forbidden(message.to_owned())
}

/// Audit privilege is explicit-only. Legacy API keys intentionally do *not* inherit these
/// capabilities from their compatibility `has_scope` behavior: granting audit access requires a
/// scoped OAuth credential containing the exact string.
fn require_capability(
    actor: &PublisherIdentity,
    capability: &str,
    tenant: &str,
) -> Result<(), AppError> {
    let scopes = actor
        .scopes
        .as_ref()
        .ok_or_else(|| audit_error("audit capability is required"))?;
    if !scopes.contains(capability) {
        return Err(audit_error("audit capability is required"));
    }
    if tenant != actor.org.0 && !scopes.contains(AUDIT_GLOBAL_CAPABILITY) {
        return Err(audit_error("global audit capability is required"));
    }
    Ok(())
}

fn tenant_for(actor: &PublisherIdentity, requested: Option<String>) -> Result<String, AppError> {
    let tenant = requested
        .unwrap_or_else(|| actor.org.0.clone())
        .trim()
        .to_owned();
    if tenant.is_empty() || tenant.len() > 80 || tenant.contains('\0') {
        return Err(AppError::Validation("invalid audit tenant".to_owned()));
    }
    Ok(tenant)
}

fn cursor_mac(key: &[u8; 32], payload: &str) -> String {
    let mut message = Vec::new();
    message.extend_from_slice(DOMAIN);
    message.extend_from_slice(b"cursor\0");
    message.extend_from_slice(payload.as_bytes());
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts 32-byte key");
    mac.update(&message);
    hex::encode(mac.finalize().into_bytes())
}

fn constant_equal(left: &str, right: &str) -> bool {
    let left = left.as_bytes();
    let right = right.as_bytes();
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right)
        .fold(0_u8, |different, (a, b)| different | (a ^ b))
        == 0
}

#[derive(Serialize, serde::Deserialize)]
struct CursorPayload {
    v: u8,
    tenant: String,
    last_sequence: u64,
}

pub(super) fn cursor_for(
    key: &[u8; 32],
    tenant: &str,
    last_sequence: u64,
) -> Result<String, AppError> {
    let payload = serde_json::to_vec(&CursorPayload {
        v: 1,
        tenant: tenant.to_owned(),
        last_sequence,
    })
    .map_err(|_| AppError::Internal)?;
    let payload = URL_SAFE_NO_PAD.encode(payload);
    Ok(format!("{payload}.{}", cursor_mac(key, &payload)))
}

pub(super) fn cursor_from(
    key: &[u8; 32],
    cursor: Option<&str>,
    tenant: &str,
) -> Result<u64, AppError> {
    let Some(cursor) = cursor.filter(|value| !value.is_empty()) else {
        return Ok(0);
    };
    let mut parts = cursor.split('.');
    let (Some(payload), Some(signature), None) = (parts.next(), parts.next(), parts.next()) else {
        return Err(AppError::Validation("invalid audit cursor".to_owned()));
    };
    if !constant_equal(signature, &cursor_mac(key, payload)) {
        return Err(AppError::Validation("invalid audit cursor".to_owned()));
    }
    let decoded = URL_SAFE_NO_PAD
        .decode(payload)
        .map_err(|_| AppError::Validation("invalid audit cursor".to_owned()))?;
    let parsed: CursorPayload = serde_json::from_slice(&decoded)
        .map_err(|_| AppError::Validation("invalid audit cursor".to_owned()))?;
    if parsed.v != 1 || parsed.tenant != tenant {
        return Err(AppError::Validation(
            "audit cursor does not match tenant".to_owned(),
        ));
    }
    Ok(parsed.last_sequence)
}

fn audit_rows(
    conn: &Connection,
    tenant: &str,
    after: u64,
    limit: u64,
) -> Result<Vec<AuditRow>, AppError> {
    let mut statement = conn.prepare("SELECT sequence,event_id,tenant,operation,target_type,target_id,result,classification,occurred_at,event_hash FROM security_audit_events WHERE tenant=?1 AND sequence>?2 ORDER BY sequence ASC LIMIT ?3").map_err(|_| AppError::Internal)?;
    statement
        .query_map(
            params![
                tenant,
                i64::try_from(after).map_err(|_| AppError::Internal)?,
                i64::try_from(limit).map_err(|_| AppError::Internal)?
            ],
            |row| {
                Ok(AuditRow {
                    sequence: u64::try_from(row.get::<_, i64>(0)?).unwrap_or(0),
                    event_id: row.get(1)?,
                    tenant: row.get(2)?,
                    operation: row.get(3)?,
                    target_type: row.get(4)?,
                    target_id: row.get(5)?,
                    result: row.get(6)?,
                    classification: row.get(7)?,
                    occurred_at: row.get(8)?,
                    event_hash: row.get(9)?,
                })
            },
        )
        .map_err(|_| AppError::Internal)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| AppError::Internal)
}

pub fn query_authorized(
    conn: &Connection,
    key: &[u8; 32],
    actor: &PublisherIdentity,
    query: AuditQuery,
) -> Result<AuditPage, AppError> {
    let tenant = tenant_for(actor, query.tenant)?;
    require_capability(actor, AUDIT_READ_CAPABILITY, &tenant)?;
    let after = cursor_from(key, query.cursor.as_deref(), &tenant)?;
    let limit = query
        .limit
        .unwrap_or(AUDIT_DEFAULT_LIMIT)
        .clamp(1, AUDIT_MAX_LIMIT);
    let mut rows = audit_rows(conn, &tenant, after, limit.saturating_add(1))?;
    let has_more = rows.len() > usize::try_from(limit).unwrap_or(usize::MAX);
    rows.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
    let next = if has_more {
        rows.last()
            .map(|row| cursor_for(key, &tenant, row.sequence))
            .transpose()?
    } else {
        None
    };
    Ok(AuditPage { events: rows, next })
}

pub fn export_authorized(
    conn: &Connection,
    key: &[u8; 32],
    actor: &PublisherIdentity,
    query: AuditExportQuery,
) -> Result<AuditExport, AppError> {
    let tenant = tenant_for(actor, query.tenant)?;
    require_capability(actor, AUDIT_EXPORT_CAPABILITY, &tenant)?;
    require_capability(actor, AUDIT_READ_CAPABILITY, &tenant)?;
    let after = cursor_from(key, query.cursor.as_deref(), &tenant)?;
    let limit = query
        .limit
        .unwrap_or(AUDIT_EXPORT_MAX_ROWS)
        .clamp(1, AUDIT_EXPORT_MAX_ROWS);
    let rows = audit_rows(conn, &tenant, after, limit.saturating_add(1))?;
    let mut ndjson = String::new();
    let mut count = 0_usize;
    let mut last_sequence = None;
    for row in rows
        .iter()
        .take(usize::try_from(limit).unwrap_or(usize::MAX))
    {
        let line = format!(
            "{}\n",
            serde_json::to_string(row).map_err(|_| AppError::Internal)?
        );
        if ndjson.len().saturating_add(line.len()) > AUDIT_EXPORT_MAX_BYTES {
            break;
        }
        ndjson.push_str(&line);
        count = count.saturating_add(1);
        last_sequence = Some(row.sequence);
    }
    let truncated = count < rows.len();
    let reason = (count == 0 && !rows.is_empty()).then_some("first_row_exceeds_export_cap");
    let next = if count > 0 && truncated {
        last_sequence
            .map(|sequence| cursor_for(key, &tenant, sequence))
            .transpose()?
    } else {
        None
    };
    Ok(AuditExport {
        bytes: ndjson.len(),
        ndjson,
        rows: count,
        truncated,
        next,
        reason,
    })
}

/// Fixed tenant predicate and bounded query primitive used by later HTTP/MCP adapters.
pub fn query_tenant(
    conn: &Connection,
    tenant: &str,
    after: u64,
    limit: u64,
) -> Result<Vec<AuditRow>, AppError> {
    let limit = limit.clamp(1, 500);
    let mut statement = conn.prepare("SELECT sequence,event_id,tenant,operation,target_type,target_id,result,classification,occurred_at,event_hash FROM security_audit_events WHERE tenant=?1 AND sequence>?2 ORDER BY sequence ASC LIMIT ?3").map_err(|_| AppError::Internal)?;
    statement
        .query_map(
            params![
                tenant,
                i64::try_from(after).map_err(|_| AppError::Internal)?,
                i64::try_from(limit).map_err(|_| AppError::Internal)?
            ],
            |row| {
                Ok(AuditRow {
                    sequence: u64::try_from(row.get::<_, i64>(0)?).unwrap_or(0),
                    event_id: row.get(1)?,
                    tenant: row.get(2)?,
                    operation: row.get(3)?,
                    target_type: row.get(4)?,
                    target_id: row.get(5)?,
                    result: row.get(6)?,
                    classification: row.get(7)?,
                    occurred_at: row.get(8)?,
                    event_hash: row.get(9)?,
                })
            },
        )
        .map_err(|_| AppError::Internal)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| AppError::Internal)
}
