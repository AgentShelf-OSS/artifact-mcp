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

use std::future::Future;

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use hmac::{Hmac, Mac};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde::Serialize;
use sha2::{Digest as _, Sha256};

use crate::config::is_valid_webhook_id;
use crate::error::AppError;
use crate::model::{OrgId, PublisherIdentity, Viewer};
use crate::persistence::db::{self, DbPool};

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

#[derive(Debug)]
struct PersistedHead {
    sequence: i64,
    key_id: String,
    head_hash: String,
    head_mac: String,
    canonical_version: i64,
    pending_receipts_root: String,
}

#[derive(Clone, Copy)]
struct ReceiptMacInput<'a> {
    correlation_id: &'a str,
    durability_intent_id: &'a str,
    state: &'a str,
    event_id: &'a str,
    context: &'a AuditContext,
    event: &'a AuditEvent,
    key_id: &'a str,
    canonical_version: i64,
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

tokio::task_local! {
    // The transport observation id is generated before parsing/authentication. It is not a
    // client field and survives every tool call made while the MCP request is in flight.
    static MCP_REQUEST_ID: String;
}

/// Scope work performed for one already-observed MCP request.  The task-local is deliberately
/// set only by the authenticated transport, never by an MCP argument.
pub async fn with_mcp_request_id<T>(request_id: String, future: impl Future<Output = T>) -> T {
    MCP_REQUEST_ID.scope(request_id, future).await
}

/// Opaque, server-derived mutation attribution. Routes may construct this only after their
/// authentication middleware has verified an identity; lifecycle code chooses the operation,
/// target and durability correlation and never accepts them from an HTTP/MCP payload.
#[derive(Clone, Debug)]
pub struct MutationAudit {
    context: AuditContext,
    request_id: String,
}

/// Private-to-the-server browser request correlation. It is deliberately distinct from the
/// outward `x-request-id`, which tower-http permits a client to supply.
#[derive(Clone, Debug)]
pub struct AuditRequestId(String);

impl AuditRequestId {
    pub(crate) fn generate() -> Result<Self, AppError> {
        MutationAudit::request_id("browser").map(Self)
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl MutationAudit {
    fn request_id(source: &str) -> Result<String, AppError> {
        let mut random = [0_u8; 16];
        getrandom::fill(&mut random).map_err(|_| AppError::Internal)?;
        Ok(format!("{source}-{}", hex::encode(random)))
    }

    pub fn publisher(publisher: &PublisherIdentity) -> Result<Self, AppError> {
        let request_id = match MCP_REQUEST_ID.try_with(Clone::clone) {
            Ok(request_id) => request_id,
            Err(_) => Self::request_id("mcp")?,
        };
        Ok(Self {
            context: AuditContext {
                tenant: publisher.org.0.clone(),
                actor_type: "api_key".to_owned(),
                actor_id: publisher.client_id.0.clone(),
                actor_role: publisher.role.clone(),
                source: "mcp".to_owned(),
                request_id: request_id.clone(),
            },
            request_id,
        })
    }

    pub fn viewer(viewer: &Viewer) -> Result<Self, AppError> {
        Self::viewer_with_request_id(viewer, None)
    }

    /// Browser route attribution with the server-generated `RequestId` when the caller is in
    /// the normal HTTP stack. Route-only tests without that middleware obtain a CSPRNG id.
    pub fn viewer_with_request_id(
        viewer: &Viewer,
        request_id: Option<&AuditRequestId>,
    ) -> Result<Self, AppError> {
        let email = viewer.email.as_ref().ok_or_else(|| {
            AppError::Validation("verified viewer identity required for audit".to_owned())
        })?;
        let org = viewer.org.as_ref().ok_or_else(|| {
            AppError::Validation("verified viewer organization required for audit".to_owned())
        })?;
        let request_id = request_id
            .map(|value| value.as_str().to_owned())
            .unwrap_or(Self::request_id("browser")?);
        Ok(Self {
            context: AuditContext {
                tenant: org.0.clone(),
                actor_type: "viewer".to_owned(),
                actor_id: email.0.clone(),
                actor_role: if viewer.is_admin { "admin" } else { "member" }.to_owned(),
                source: "browser".to_owned(),
                request_id: request_id.clone(),
            },
            request_id,
        })
    }

    pub fn recovery() -> Result<Self, AppError> {
        let request_id = Self::request_id("reconciliation")?;
        Ok(Self {
            context: AuditContext {
                tenant: "system".to_owned(),
                actor_type: "system".to_owned(),
                actor_id: "artifact-mcp".to_owned(),
                actor_role: String::new(),
                source: "reconciliation".to_owned(),
                request_id: request_id.clone(),
            },
            request_id,
        })
    }

    pub fn maintenance() -> Result<Self, AppError> {
        let request_id = Self::request_id("maintenance")?;
        Ok(Self {
            context: AuditContext {
                tenant: "system".to_owned(),
                actor_type: "system".to_owned(),
                actor_id: "artifact-mcp".to_owned(),
                actor_role: String::new(),
                source: "maintenance".to_owned(),
                request_id: request_id.clone(),
            },
            request_id,
        })
    }

    /// Scope a verified browser administrator's action to the organization it mutates.  This is
    /// server-only: callers cannot choose an actor or elevate an ordinary member into another
    /// tenant's ledger stream.
    pub fn admin_for_tenant(&self, tenant: &str) -> Result<Self, AppError> {
        if self.context.actor_type != "viewer" || self.context.actor_role != "admin" {
            return Err(AppError::Forbidden(
                "administrator audit context required".to_owned(),
            ));
        }
        let tenant = id(tenant, true)?;
        let mut context = self.context.clone();
        context.tenant = tenant;
        Ok(Self {
            context,
            request_id: self.request_id.clone(),
        })
    }

    /// Keep an actor in its own tenant, or require verified browser-admin authority for a
    /// cross-tenant target. Persistence uses this rather than falling back to a home tenant.
    pub fn for_target_tenant(&self, tenant: &str) -> Result<Self, AppError> {
        if self.context.tenant == tenant {
            return Ok(self.clone());
        }
        self.admin_for_tenant(tenant)
    }

    #[must_use]
    pub fn context(&self) -> &AuditContext {
        &self.context
    }

    #[must_use]
    pub fn correlation(&self, operation: &str, target_id: &str, revision: Option<u64>) -> String {
        format!(
            "audit:{operation}:{target_id}:{}:{}",
            revision.map_or_else(|| "new".to_owned(), |value| value.to_string()),
            self.request_id
        )
    }

    pub fn event_id(&self) -> Result<String, AppError> {
        // A request can complete more than one independent mutation.  The terminal record ID
        // must therefore be unique per receipt, while the request id remains the correlation.
        Self::request_id("audit")
    }

    /// Re-scope an already-verified administrator action to the tenant of the record being
    /// changed.  The tenant comes from persisted metadata, never a browser/MCP argument, so a
    /// global administrator's home organization cannot misfile a cross-organization audit row.
    pub(crate) fn for_affected_tenant(&self, tenant: &OrgId) -> Self {
        let mut context = self.context.clone();
        context.tenant.clone_from(&tenant.0);
        Self {
            context,
            request_id: self.request_id.clone(),
        }
    }
}

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

/// One runtime-owned access service. It intentionally owns only a pool and the already-validated
/// HMAC key; it never carries an actor credential or exposes the key in Debug/display.
#[derive(Clone)]
pub struct AuditAccess {
    pool: DbPool,
    key: [u8; 32],
}

impl std::fmt::Debug for AuditAccess {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("AuditAccess(<redacted>)")
    }
}

impl AuditAccess {
    #[must_use]
    pub const fn new(pool: DbPool, key: [u8; 32]) -> Self {
        Self { pool, key }
    }

    pub async fn query(
        &self,
        actor: &PublisherIdentity,
        query: AuditQuery,
    ) -> Result<AuditPage, AppError> {
        let pool = self.pool.clone();
        let key = self.key;
        let actor = actor.clone();
        db::interact(&pool, move |conn| {
            query_authorized(conn, &key, &actor, query)
        })
        .await
    }

    pub async fn export(
        &self,
        actor: &PublisherIdentity,
        query: AuditExportQuery,
    ) -> Result<AuditExport, AppError> {
        let pool = self.pool.clone();
        let key = self.key;
        let actor = actor.clone();
        db::interact(&pool, move |conn| {
            export_authorized(conn, &key, &actor, query)
        })
        .await
    }

    /// Retention is deliberately an operator-only API, not an MCP tool. Its caller must arrange
    /// scheduling outside this process; every execution verifies the complete current ledger.
    pub async fn prune_expired(&self) -> Result<u64, AppError> {
        let pool = self.pool.clone();
        let key = self.key;
        db::interact(&pool, move |conn| prune_expired(conn, &key, None, 1_000)).await
    }
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

fn id(value: &str, required: bool) -> Result<String, AppError> {
    let value = value.trim();
    if (required && value.is_empty())
        || value.len() > 100
        || !value.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_' | b'-')
        })
    {
        return Err(AppError::Validation(
            "audit fields must be server-controlled identifiers".to_owned(),
        ));
    }
    Ok(value.to_owned())
}

fn bounded(value: &str, maximum: usize) -> Result<String, AppError> {
    let value = value.trim();
    if value.len() > maximum || value.contains('\0') {
        return Err(AppError::Validation("invalid audit context".to_owned()));
    }
    Ok(value.to_owned())
}

fn canonical(
    event_id: &str,
    context: &AuditContext,
    event: &AuditEvent,
    occurred_at: &str,
) -> Result<Vec<u8>, AppError> {
    if !matches!(context.actor_type.as_str(), "api_key" | "viewer" | "system")
        || !matches!(
            context.source.as_str(),
            "mcp" | "browser" | "maintenance" | "reconciliation"
        )
        || !matches!(
            event.result.as_str(),
            "success" | "denied" | "failure" | "recovered"
        )
    {
        return Err(AppError::Validation(
            "invalid server-derived audit context or result".to_owned(),
        ));
    }
    let target_id = event.target_id.trim();
    let target_type = event.target_type.to_ascii_lowercase();
    if !target_id.is_empty() && target_type.contains("share") {
        return Err(AppError::Validation(
            "audit events must not retain share tokens".to_owned(),
        ));
    }
    if !target_id.is_empty() && target_type.contains("webhook") && !is_valid_webhook_id(target_id) {
        return Err(AppError::Validation(
            "audit events must not retain unvalidated webhook identifiers".to_owned(),
        ));
    }
    let revision = event
        .revision
        .map_or_else(String::new, |value| value.to_string());
    let fields = [
        id(event_id, true)?,
        id(KEY_ID, true)?,
        bounded(&context.tenant, 80)?,
        bounded(&context.actor_type, 20)?,
        bounded(&context.actor_id, 160)?,
        bounded(&context.actor_role, 40)?,
        id(&event.operation, true)?,
        id(&event.target_type, true)?,
        id(&event.target_id, false)?,
        id(&event.result, true)?,
        id(&event.classification, false)?,
        bounded(&context.source, 20)?,
        bounded(&context.request_id, 120)?,
        id(&revision, false)?,
        occurred_at.trim().to_owned(),
    ];
    let mut output = vec![VERSION];
    for field in fields {
        let bytes = field.as_bytes();
        output.extend_from_slice(
            &(u32::try_from(bytes.len()).map_err(|_| AppError::Internal)?).to_be_bytes(),
        );
        output.extend_from_slice(bytes);
    }
    Ok(output)
}

/// Frozen receipt snapshot format. Pending rows bind an empty event id; finalization atomically
/// binds the terminal state and assigned event id along with every original reservation field.
fn canonical_receipt_bytes(input: ReceiptMacInput<'_>) -> Result<Vec<u8>, AppError> {
    // Reuse the event admission policy so a receipt cannot persist data the terminal ledger
    // would reject. Receipt identifiers have their own syntax (`:` is expected) but remain
    // bounded server-controlled values.
    let _ = canonical("receipt", input.context, input.event, "")?;
    if input.correlation_id.is_empty()
        || input.correlation_id.len() > 240
        || input.correlation_id.contains('\0')
        || input.durability_intent_id.is_empty()
        || input.durability_intent_id.len() > 240
        || input.durability_intent_id.contains('\0')
    {
        return Err(AppError::Validation(
            "invalid audit receipt identifier".to_owned(),
        ));
    }
    if !matches!(input.state, "pending" | "finalized")
        || (input.state == "pending" && !input.event_id.is_empty())
        || (input.state == "finalized" && input.event_id.is_empty())
    {
        return Err(AppError::Validation(
            "invalid audit receipt state".to_owned(),
        ));
    }
    let event_id = id(input.event_id, input.state == "finalized")?;
    let version = u8::try_from(input.canonical_version).map_err(|_| AppError::Internal)?;
    let revision = input
        .event
        .revision
        .map_or_else(String::new, |value| value.to_string());
    let canonical_version = input.canonical_version.to_string();
    let fields = [
        input.correlation_id,
        input.durability_intent_id,
        input.state,
        event_id.as_str(),
        input.context.tenant.as_str(),
        input.context.actor_type.as_str(),
        input.context.actor_id.as_str(),
        input.context.actor_role.as_str(),
        input.context.source.as_str(),
        input.context.request_id.as_str(),
        input.event.operation.as_str(),
        input.event.target_type.as_str(),
        input.event.target_id.as_str(),
        input.event.result.as_str(),
        input.event.classification.as_str(),
        revision.as_str(),
        input.key_id,
        canonical_version.as_str(),
    ];
    let mut output = vec![version];
    for field in fields {
        let bytes = field.as_bytes();
        output.extend_from_slice(
            &(u32::try_from(bytes.len()).map_err(|_| AppError::Internal)?).to_be_bytes(),
        );
        output.extend_from_slice(bytes);
    }
    Ok(output)
}

fn receipt_message(input: ReceiptMacInput<'_>) -> Result<Vec<u8>, AppError> {
    let canonical = canonical_receipt_bytes(input)?;
    let mut message = Vec::with_capacity(DOMAIN.len() + b"receipt\0".len() + canonical.len());
    message.extend_from_slice(DOMAIN);
    message.extend_from_slice(b"receipt\0");
    message.extend_from_slice(&canonical);
    Ok(message)
}

fn receipt_mac(
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
fn hash(key: &[u8; 32], sequence: u64, previous: &str, canonical: &[u8]) -> String {
    let mut message = Vec::new();
    message.extend_from_slice(DOMAIN);
    message.extend_from_slice(&sequence.to_be_bytes());
    part(&mut message, previous.as_bytes());
    part(&mut message, canonical);
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts 32-byte key");
    mac.update(&message);
    hex::encode(mac.finalize().into_bytes())
}

fn pending_receipts_root(conn: &Connection) -> Result<String, AppError> {
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

fn head_mac(key: &[u8; 32], sequence: u64, head: &str, pending_root: &str) -> String {
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

fn load_head(conn: &Connection) -> Result<PersistedHead, AppError> {
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

fn verify_head_mac(key: &[u8; 32], head: &PersistedHead) -> Result<u64, AppError> {
    let sequence = u64::try_from(head.sequence).map_err(|_| AppError::Internal)?;
    if head.key_id != KEY_ID
        || head.canonical_version != i64::from(VERSION)
        || head.head_mac != head_mac(key, sequence, &head.head_hash, &head.pending_receipts_root)
    {
        return Err(AppError::Internal);
    }
    Ok(sequence)
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

fn checkpoint_mac(key: &[u8; 32], first: u64, last: u64, bridge: &str, previous: &str) -> String {
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

fn cursor_for(key: &[u8; 32], tenant: &str, last_sequence: u64) -> Result<String, AppError> {
    let payload = serde_json::to_vec(&CursorPayload {
        v: 1,
        tenant: tenant.to_owned(),
        last_sequence,
    })
    .map_err(|_| AppError::Internal)?;
    let payload = URL_SAFE_NO_PAD.encode(payload);
    Ok(format!("{payload}.{}", cursor_mac(key, &payload)))
}

fn cursor_from(key: &[u8; 32], cursor: Option<&str>, tenant: &str) -> Result<u64, AppError> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    use crate::persistence::migrations::{self, MigrationContext};

    fn key() -> [u8; 32] {
        [7; 32]
    }

    fn ledger() -> Connection {
        let mut conn = Connection::open_in_memory().expect("memory db");
        crate::persistence::migrations::apply(
            &mut conn,
            &crate::persistence::migrations::MigrationContext::default(),
        )
        .expect("migrations");
        initialize_head(&conn, &key()).expect("head");
        conn
    }

    fn scoped(org: &str, scopes: &[&str]) -> PublisherIdentity {
        PublisherIdentity {
            client_id: "auditor".into(),
            org: org.into(),
            label: "audit".to_owned(),
            role: "reader".to_owned(),
            scopes: Some(
                scopes
                    .iter()
                    .map(|scope| (*scope).to_owned())
                    .collect::<BTreeSet<_>>(),
            ),
        }
    }

    fn append(conn: &mut Connection, tenant: &str, id: &str) {
        let tx = conn.transaction().expect("transaction");
        let mut ctx = context("mcp");
        ctx.tenant = tenant.to_owned();
        let _ =
            append_in_transaction(&tx, &key(), id, &ctx, &event("artifact", "a1")).expect("append");
        tx.commit().expect("commit");
    }

    fn context(source: &str) -> AuditContext {
        AuditContext {
            tenant: "acme".to_owned(),
            actor_type: "api_key".to_owned(),
            actor_id: "key-1".to_owned(),
            actor_role: "author".to_owned(),
            source: source.to_owned(),
            request_id: "request-1".to_owned(),
        }
    }

    fn event(target_type: &str, target_id: &str) -> AuditEvent {
        AuditEvent {
            operation: "artifact.publish".to_owned(),
            target_type: target_type.to_owned(),
            target_id: target_id.to_owned(),
            result: "success".to_owned(),
            classification: String::new(),
            revision: Some(1),
        }
    }

    fn database() -> Connection {
        let mut conn = Connection::open_in_memory().expect("in-memory database");
        migrations::apply(&mut conn, &MigrationContext::empty()).expect("apply migrations");
        conn
    }

    fn reserve(conn: &mut Connection, key: &[u8; 32]) {
        let tx = conn.transaction().expect("reservation transaction");
        assert!(
            reserve_receipt_in_transaction(
                &tx,
                key,
                "durability:abc",
                "publish:abc:1",
                &context("mcp"),
                &event("artifact", "abc"),
            )
            .expect("reserve authenticated receipt")
        );
        tx.commit().expect("commit receipt");
    }

    #[test]
    fn v25_sources_and_redacted_target_types_match_node() {
        assert!(canonical("event-1", &context("worker"), &event("artifact", "a1"), "").is_err());
        assert!(
            canonical(
                "event-1",
                &context("mcp"),
                &event("public_share", "token"),
                ""
            )
            .is_err()
        );
        assert!(
            canonical(
                "event-1",
                &context("mcp"),
                &event("webhook", "wh0000000001"),
                ""
            )
            .is_ok()
        );
        assert!(
            canonical(
                "event-1",
                &context("mcp"),
                &event("webhook", "https://secret"),
                ""
            )
            .is_err()
        );
        assert!(
            canonical(
                "event-1",
                &context("mcp"),
                &event("webhook", "a-real-webhook-token"),
                ""
            )
            .is_err()
        );
        assert!(canonical("event-1", &context("mcp"), &event("artifact", "a1"), "").is_ok());
    }

    #[test]
    fn terminal_event_ids_are_distinct_from_request_correlation() {
        let publisher = PublisherIdentity {
            client_id: "key-1".into(),
            org: "acme".into(),
            label: "test".to_owned(),
            role: "admin".to_owned(),
            scopes: None,
        };
        let audit = MutationAudit::publisher(&publisher).expect("random audit context");
        let first = audit.event_id().expect("random terminal id");
        let second = audit.event_id().expect("random terminal id");
        assert_ne!(first, second);
        assert!(
            audit
                .correlation("artifact.publish", "a1", Some(1))
                .contains(audit.context().request_id.as_str())
        );
    }

    #[test]
    fn receipt_mac_authenticates_every_reserved_field() {
        let key = [7_u8; 32];
        let cases = [
            ("correlation_id", "durability:tampered"),
            ("durability_intent_id", "publish:other:1"),
            ("state", "finalized"),
            ("event_id", "event-other"),
            ("tenant", "other"),
            ("actor_type", "viewer"),
            ("actor_id", "other-agent"),
            ("actor_role", "admin"),
            ("source", "browser"),
            ("request_id", "request-2"),
            ("operation", "artifact.update"),
            ("target_type", "document"),
            ("target_id", "other"),
            ("result", "denied"),
            ("classification", "policy"),
            ("revision", "2"),
            ("key_id", "v2"),
            ("canonical_version", "2"),
            (
                "receipt_mac",
                "0000000000000000000000000000000000000000000000000000000000000000",
            ),
        ];
        for (field, value) in cases {
            let mut conn = database();
            reserve(&mut conn, &key);
            conn.execute(
                &format!("UPDATE security_audit_receipts SET {field}=?1 WHERE correlation_id=?2"),
                params![value, "durability:abc"],
            )
            .expect("tamper receipt");
            let correlation = if field == "correlation_id" {
                value
            } else {
                "durability:abc"
            };
            let tx = conn.transaction().expect("finalization transaction");
            assert!(
                finalize_receipt_in_transaction(
                    &tx,
                    &key,
                    correlation,
                    "event-1",
                    &context("mcp"),
                    &event("artifact", "abc"),
                )
                .is_err(),
                "{field} tampering must fail closed"
            );
            drop(tx);
            assert_eq!(
                conn.query_row("SELECT COUNT(*) FROM security_audit_events", [], |row| row
                    .get::<_, i64>(
                    0
                ))
                .expect("event count"),
                0,
                "{field} tampering must not append an event"
            );
        }
    }

    #[test]
    fn receipt_mac_matches_the_node_protocol_vector() {
        assert_eq!(
            receipt_mac(
                &[7_u8; 32],
                "durability:abc",
                "publish:abc:1",
                "pending",
                "",
                &context("mcp"),
                &event("artifact", "abc"),
            )
            .expect("receipt MAC"),
            "c31530275592868cd7ed2070e9f8d0da3f827905bb555283ffe6d8b2ac6af9b3"
        );
    }

    #[test]
    fn pending_receipt_root_matches_the_node_protocol_vectors() {
        let mut conn = database();
        assert_eq!(
            pending_receipts_root(&conn).expect("empty pending root"),
            "8826f8e30ad491deb7642729c14a19fde13144b8f1b1d15e4eca84585f18be53"
        );
        reserve(&mut conn, &[7_u8; 32]);
        assert_eq!(
            pending_receipts_root(&conn).expect("one-receipt pending root"),
            "faabbef514cca7b567de7b20dc04ee845749efa0ef1cbf76b86ad784ec984b14"
        );
    }

    #[test]
    fn reconciliation_and_compensation_reject_tampered_receipts() {
        let key = [7_u8; 32];
        let mut reconciliation = database();
        reserve(&mut reconciliation, &key);
        reconciliation
            .execute(
                "UPDATE security_audit_receipts SET tenant='other' WHERE durability_intent_id='publish:abc:1'",
                [],
            )
            .expect("tamper reconciliation receipt");
        let tx = reconciliation
            .transaction()
            .expect("reconciliation transaction");
        assert!(
            finalize_reconciled_receipt_in_transaction(
                &tx,
                &key,
                "publish:abc:1",
                "event-1",
                "recovered",
                "",
            )
            .is_err()
        );
        drop(tx);
        assert_eq!(
            reconciliation
                .query_row("SELECT COUNT(*) FROM security_audit_events", [], |row| row
                    .get::<_, i64>(
                    0
                ))
                .expect("event count"),
            0
        );

        let mut compensation = database();
        reserve(&mut compensation, &key);
        compensation
            .execute(
                "UPDATE security_audit_receipts SET target_id='other' WHERE durability_intent_id='publish:abc:1'",
                [],
            )
            .expect("tamper compensation receipt");
        let tx = compensation
            .transaction()
            .expect("compensation transaction");
        assert!(
            verify_pending_receipt_in_transaction(&tx, &key, "publish:abc:1").is_err(),
            "compensation must authenticate before deleting"
        );
        drop(tx);
        assert_eq!(
            compensation
                .query_row("SELECT COUNT(*) FROM security_audit_receipts", [], |row| {
                    row.get::<_, i64>(0)
                })
                .expect("receipt count"),
            1
        );
    }

    #[test]
    fn only_verified_browser_administrators_can_scope_to_another_tenant() {
        let admin = Viewer {
            email: Some("admin@example.test".into()),
            org: Some("home".into()),
            is_admin: true,
        };
        let member = Viewer {
            email: Some("member@example.test".into()),
            org: Some("home".into()),
            is_admin: false,
        };
        let scoped = MutationAudit::viewer(&admin)
            .expect("admin audit")
            .admin_for_tenant("target")
            .expect("admin can scope");
        assert_eq!(scoped.context().tenant, "target");
        assert!(
            MutationAudit::viewer(&member)
                .expect("member audit")
                .admin_for_tenant("target")
                .is_err()
        );
    }

    #[test]
    fn audit_access_is_explicit_cursor_bound_and_cross_tenant_denied() {
        let mut conn = ledger();
        append(&mut conn, "acme", "event-1");
        append(&mut conn, "acme", "event-2");
        append(&mut conn, "beta", "event-3");
        let reader = scoped("acme", &[AUDIT_READ_CAPABILITY]);
        assert!(
            query_authorized(
                &conn,
                &key(),
                &reader,
                AuditQuery {
                    tenant: None,
                    cursor: None,
                    limit: Some(1)
                }
            )
            .is_ok()
        );
        let page = query_authorized(
            &conn,
            &key(),
            &reader,
            AuditQuery {
                tenant: None,
                cursor: None,
                limit: Some(1),
            },
        )
        .expect("page");
        assert_eq!(page.events.len(), 1);
        assert!(page.next.is_some());
        assert!(
            query_authorized(
                &conn,
                &key(),
                &reader,
                AuditQuery {
                    tenant: None,
                    cursor: Some(format!("{}x", page.next.as_deref().expect("cursor"))),
                    limit: None
                }
            )
            .is_err()
        );
        assert!(
            query_authorized(
                &conn,
                &key(),
                &reader,
                AuditQuery {
                    tenant: Some("beta".to_owned()),
                    cursor: page.next.clone(),
                    limit: None
                }
            )
            .is_err()
        );
        let global = scoped("acme", &[AUDIT_READ_CAPABILITY, AUDIT_GLOBAL_CAPABILITY]);
        assert_eq!(
            query_authorized(
                &conn,
                &key(),
                &global,
                AuditQuery {
                    tenant: Some("beta".to_owned()),
                    cursor: None,
                    limit: None
                }
            )
            .expect("global")
            .events
            .len(),
            1
        );
        assert!(
            query_authorized(
                &conn,
                &key(),
                &PublisherIdentity {
                    scopes: None,
                    ..reader
                },
                AuditQuery::default()
            )
            .is_err()
        );
    }

    #[test]
    fn deleted_pending_receipt_breaks_the_head_commitment_before_legacy_bypass() {
        let key = [7_u8; 32];
        let mut conn = database();
        reserve(&mut conn, &key);
        conn.execute(
            "DELETE FROM security_audit_receipts WHERE durability_intent_id='publish:abc:1'",
            [],
        )
        .expect("delete pending receipt");
        assert!(!verify(&conn, &key).expect("verify ledger"));
        let tx = conn.transaction().expect("reconciliation transaction");
        assert!(
            finalize_reconciled_receipt_in_transaction(
                &tx,
                &key,
                "publish:abc:1",
                "event-1",
                "recovered",
                "reconciliation",
            )
            .is_err(),
            "missing authenticated receipt must not be treated as legacy"
        );
    }

    #[test]
    fn export_is_bounded_and_retention_checkpoints_a_contiguous_prefix() {
        let mut conn = ledger();
        append(&mut conn, "acme", "old-1");
        append(&mut conn, "acme", "new");
        append(&mut conn, "acme", "old-after-new");
        let reader = scoped("acme", &[AUDIT_READ_CAPABILITY, AUDIT_EXPORT_CAPABILITY]);
        let exported = export_authorized(
            &conn,
            &key(),
            &reader,
            AuditExportQuery {
                tenant: None,
                cursor: None,
                limit: Some(10_001),
            },
        )
        .expect("export");
        assert_eq!(exported.rows, 3);
        assert!(exported.ndjson.ends_with('\n'));
        assert_eq!(
            prune_expired(&mut conn, &key(), Some("9999-01-01T00:00:00Z"), 1_000).expect("prune"),
            3
        );
        assert!(verify(&conn, &key()).expect("verify"));
        assert_eq!(query_tenant(&conn, "acme", 0, 500).expect("rows").len(), 0);
    }

    #[test]
    fn finalized_duplicate_validates_the_referenced_event_projection() {
        let key = [7_u8; 32];
        let mut conn = database();
        reserve(&mut conn, &key);
        let tx = conn.transaction().expect("finalization transaction");
        finalize_receipt_in_transaction(
            &tx,
            &key,
            "durability:abc",
            "event-1",
            &context("mcp"),
            &event("artifact", "abc"),
        )
        .expect("finalize receipt");
        tx.commit().expect("commit finalization");
        conn.execute(
            "UPDATE security_audit_events SET target_id='other' WHERE event_id='event-1'",
            [],
        )
        .expect("tamper referenced event");
        let tx = conn.transaction().expect("duplicate transaction");
        assert!(
            finalize_receipt_in_transaction(
                &tx,
                &key,
                "durability:abc",
                "event-2",
                &context("mcp"),
                &event("artifact", "abc"),
            )
            .is_err(),
            "a receipt must not point at a mismatched terminal projection"
        );
    }
}
