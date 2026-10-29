//! PBI-058 durable security-audit ledger primitives.
//!
//! This is the Rust twin of `lib/audit.js`. It intentionally accepts only server-derived
//! contexts and fixed identifiers; request bodies and credentials never cross this boundary.
//!
//! Failure policy is deliberately non-oracular:
//! - verified successful SQLite mutations append exactly one terminal event in the business
//!   transaction; append/key failures roll back the mutation;
//! - validation failures, missing/no-op targets, unauthenticated requests, and concealed probes
//!   append nothing, because a ledger row could confirm a probed identifier;
//! - terminal `denied`/`failure` rows are allowed only after authorization and target disclosure
//!   are established, so they cannot create an existence oracle;
//! - the authorized webhook-test exception commits a redacted requested marker before external
//!   I/O and one correlated terminal success/failure afterward. A crash can leave requested-only
//!   evidence and a manual retry may redeliver, but the ledger never silently erases the attempt
//!   or claims external atomicity/exactly-once delivery.

mod access;
mod append;
mod attribution;
mod chain;
mod encoding;
mod query;
mod receipts;
mod retention;
mod schema;
mod verify;

#[cfg(test)]
mod characterization_tests;
#[cfg(test)]
mod tests;

use hmac::Hmac;
use sha2::Sha256;

pub use access::AuditAccess;
pub use append::{append_in_transaction, mutate_in_transaction};
pub use attribution::{AuditRequestId, MutationAudit, with_mcp_request_id};
pub use chain::{initialize_head, parse_hmac_key};
pub use query::{
    AuditExport, AuditExportQuery, AuditPage, AuditQuery, export_authorized, query_authorized,
    query_tenant,
};
pub use receipts::{
    delete_pending_receipt_in_transaction, finalize_receipt_in_transaction,
    finalize_reconciled_receipt_in_transaction, reserve_receipt_in_transaction,
    verify_pending_receipt_in_transaction, verify_pending_receipts,
};
pub use retention::prune_expired;
pub use schema::{AuditContext, AuditEvent, AuditRow};
pub use verify::verify;

const VERSION: u8 = 1;
const DOMAIN: &[u8] = b"artifact-mcp/security-audit/v1\0";
const KEY_ID: &str = "v1";
pub const AUDIT_RETENTION_DAYS: i64 = 180;
pub const AUDIT_DEFAULT_LIMIT: u64 = 100;
pub const AUDIT_MAX_LIMIT: u64 = 500;
pub const AUDIT_EXPORT_MAX_ROWS: u64 = 10_000;
pub const AUDIT_EXPORT_MAX_BYTES: usize = 5 * 1024 * 1024;
pub const AUDIT_READ_CAPABILITY: &str = "audit:read";
pub const AUDIT_EXPORT_CAPABILITY: &str = "audit:export";
pub const AUDIT_GLOBAL_CAPABILITY: &str = "audit:global";

type HmacSha256 = Hmac<Sha256>;
