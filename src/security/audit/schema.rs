//! Public ledger entry shapes: the verified actor projection, the redacted operation, and
//! the safe row projection returned by readers.

use serde::Serialize;

/// A verified actor projection, never constructed from raw request identity fields.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuditContext {
    pub tenant: String,
    pub actor_type: String,
    pub actor_id: String,
    pub actor_role: String,
    pub source: String,
    pub request_id: String,
}

/// A redacted, fixed-shape security operation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuditEvent {
    pub operation: String,
    pub target_type: String,
    pub target_id: String,
    pub result: String,
    pub classification: String,
    pub revision: Option<u64>,
}

/// Persisted safe projection returned by bounded readers.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct AuditRow {
    pub sequence: u64,
    pub event_id: String,
    pub tenant: String,
    pub operation: String,
    pub target_type: String,
    pub target_id: String,
    pub result: String,
    pub classification: String,
    pub occurred_at: String,
    pub event_hash: String,
}
