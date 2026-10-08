//! ADR-0012 scheduled reminders and Web Push subscriptions.
//!
//! The Node reference `lib/push.js` stores and validates the same rows; only this runtime runs
//! the sweeper and the sender. Every function here is synchronous `rusqlite` work: callers run it
//! inside `spawn_blocking` (see [`crate::persistence::db::interact`]).

use base64::{
    Engine as _,
    engine::general_purpose::{STANDARD as BASE64_STANDARD, URL_SAFE_NO_PAD},
};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::{
    error::AppError,
    mcp::protocol::OrderedJson,
    persistence::{migrations::EncryptedUrl, state},
};

pub use crate::persistence::state::StateScope as ReminderScope;

/// At most 16 `armed` rows per artifact, all scopes together.
pub const ARMED_REMINDER_LIMIT: i64 = 16;
/// Registering an 11th subscription deletes the viewer's oldest row.
pub const SUBSCRIPTIONS_PER_VIEWER: usize = 10;
/// The earliest permitted fire time, relative to the server clock.
pub const MIN_LEAD_MS: i64 = 60 * 1000;
/// The latest permitted fire time, relative to the server clock.
pub const MAX_LEAD_MS: i64 = 30 * 24 * 60 * 60 * 1000;
/// A reminder this late is marked fired without delivery.
pub const STALE_AFTER_MS: i64 = 6 * 60 * 60 * 1000;
/// A delivery expires this long after it is queued.
pub const DELIVERY_TTL_MS: i64 = 30 * 60 * 1000;
/// Due reminders claimed per sweeper transaction.
pub const SWEEP_BATCH: i64 = 100;
/// Title limit in Unicode scalar values.
pub const TITLE_MAX_CHARS: usize = 80;
/// Body limit in Unicode scalar values.
pub const BODY_MAX_CHARS: usize = 240;
/// Endpoint limit in UTF-8 bytes.
pub const ENDPOINT_MAX_BYTES: usize = 1024;
/// Device label limit in Unicode scalar values.
pub const LABEL_MAX_CHARS: usize = 60;
/// Tool error text when the feature is off.
pub const PUSH_DISABLED_MESSAGE: &str = "Web Push reminders are not configured on this server.";

const JS_MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;

/// Shared route/MCP validation and write failures, with their wire codes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PushError {
    #[error("bad_scope")]
    BadScope,
    #[error("bad_key")]
    BadKey,
    #[error("bad_time")]
    BadTime,
    #[error("bad_text")]
    BadText,
    #[error("reminder_limit")]
    ReminderLimit,
    #[error("bad_endpoint")]
    BadEndpoint,
    #[error("bad_keys")]
    BadKeys,
    #[error("push_unavailable")]
    Unavailable,
}

impl PushError {
    /// The JSON `error` code.
    #[must_use]
    pub fn code(self) -> &'static str {
        match self {
            Self::BadScope => "bad_scope",
            Self::BadKey => "bad_key",
            Self::BadTime => "bad_time",
            Self::BadText => "bad_text",
            Self::ReminderLimit => "reminder_limit",
            Self::BadEndpoint => "bad_endpoint",
            Self::BadKeys => "bad_keys",
            Self::Unavailable => "push_unavailable",
        }
    }

    /// The HTTP status for the route surface.
    #[must_use]
    pub const fn status(self) -> u16 {
        match self {
            Self::ReminderLimit => 409,
            Self::Unavailable => 500,
            _ => 400,
        }
    }
}

impl From<AppError> for PushError {
    fn from(_: AppError) -> Self {
        Self::Unavailable
    }
}

fn db_error(error: rusqlite::Error) -> AppError {
    tracing::error!(error = %error, "push persistence failed");
    AppError::Internal
}

// ---------------------------------------------------------------------------
// Validation shared by routes and MCP
// ---------------------------------------------------------------------------

/// `^[A-Za-z0-9._-]{1,64}$`, identical to viewer state keys.
#[must_use]
pub fn valid_key(key: &str) -> bool {
    state::valid_key(key)
}

fn js_trim(value: &str) -> &str {
    value.trim_matches(|c: char| c == '\u{feff}' || (c.is_whitespace() && c != '\u{85}'))
}

fn forbidden_control(value: &str) -> bool {
    value.chars().any(|c| c.is_control() && c != '\n')
}

fn safe_integer(value: &OrderedJson) -> Option<i64> {
    match value {
        OrderedJson::Number(number) => number
            .as_f64()
            .filter(|v| v.fract() == 0.0 && v.abs() <= JS_MAX_SAFE_INTEGER)
            .map(|v| v as i64),
        _ => None,
    }
}

/// `reminderFireAt` — exactly one of `fire_at` (epoch ms) or `delay_seconds`, resolved to a time
/// 60 seconds to 30 days after `now_ms`.
///
/// # Errors
/// [`PushError::BadTime`] for every other input. A field that is present but `null` counts as
/// present and is not a safe integer, so it is `bad_time`.
pub fn resolve_fire_at(
    fire_at: Option<&OrderedJson>,
    delay_seconds: Option<&OrderedJson>,
    now_ms: i64,
) -> Result<i64, PushError> {
    let resolved = match (fire_at, delay_seconds) {
        (Some(fire_at), None) => safe_integer(fire_at).ok_or(PushError::BadTime)?,
        (None, Some(delay)) => {
            let delay = safe_integer(delay).ok_or(PushError::BadTime)?;
            delay
                .checked_mul(1000)
                .and_then(|delay| now_ms.checked_add(delay))
                .ok_or(PushError::BadTime)?
        }
        _ => return Err(PushError::BadTime),
    };
    let lead = resolved.saturating_sub(now_ms);
    if !(MIN_LEAD_MS..=MAX_LEAD_MS).contains(&lead) {
        return Err(PushError::BadTime);
    }
    Ok(resolved)
}

/// Validated reminder text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReminderText {
    pub title: String,
    pub body: String,
}

/// `reminderText(title, body)`: a trimmed title of 1..=80 characters and a body of 0..=240
/// characters, with no control characters other than `\n`.
///
/// # Errors
/// [`PushError::BadText`] for every other input, including a non-string or `null` body.
pub fn validate_text(
    title: Option<&OrderedJson>,
    body: Option<&OrderedJson>,
) -> Result<ReminderText, PushError> {
    let Some(OrderedJson::String(title)) = title else {
        return Err(PushError::BadText);
    };
    let body = match body {
        None => "",
        Some(OrderedJson::String(body)) => body.as_str(),
        Some(_) => return Err(PushError::BadText),
    };
    let title = js_trim(title);
    let title_chars = title.chars().count();
    if !(1..=TITLE_MAX_CHARS).contains(&title_chars)
        || body.chars().count() > BODY_MAX_CHARS
        || forbidden_control(title)
        || forbidden_control(body)
    {
        return Err(PushError::BadText);
    }
    Ok(ReminderText {
        title: title.to_owned(),
        body: body.to_owned(),
    })
}

/// Device label: at most 60 characters and no control characters.
#[must_use]
pub fn valid_label(label: &str) -> bool {
    label.chars().count() <= LABEL_MAX_CHARS && !label.chars().any(char::is_control)
}

/// `hostAllowed(hostname, allowlist)`: exact names, or `*.suffix` entries matching subdomains only.
#[must_use]
pub fn host_allowed(host: &str, allowlist: &[String]) -> bool {
    let host = host.to_ascii_lowercase();
    let unbracketed = host.trim_start_matches('[').trim_end_matches(']');
    if host.is_empty() || unbracketed.parse::<std::net::IpAddr>().is_ok() {
        return false;
    }
    allowlist.iter().any(|entry| match entry.strip_prefix('*') {
        Some(dot_suffix) if dot_suffix.starts_with('.') => {
            host.ends_with(dot_suffix) && host.len() > dot_suffix.len()
        }
        _ => host == *entry,
    })
}

/// `validPushEndpoint`: an absolute `https:` URL of at most 1024 bytes, with no userinfo, on the
/// default port, whose host is on the allowlist. The check runs on save and before every send.
#[must_use]
pub fn valid_endpoint(endpoint: &str, allowlist: &[String]) -> bool {
    if endpoint.is_empty() || endpoint.len() > ENDPOINT_MAX_BYTES {
        return false;
    }
    let Ok(url) = url::Url::parse(endpoint) else {
        return false;
    };
    url.scheme() == "https"
        && url.username().is_empty()
        && url.password().is_none()
        // The URL parser drops an explicit default port, so `:443` also yields `None`.
        && url.port().is_none()
        && matches!(url.host(), Some(url::Host::Domain(host)) if host_allowed(host, allowlist))
}

/// Decode canonical base64url (optional `=` padding), as `base64urlBytes` in `lib/push.js`.
#[must_use]
pub fn base64url_bytes(value: &str) -> Option<Vec<u8>> {
    let unpadded = value.trim_end_matches('=');
    if value.len() - unpadded.len() > 2
        || unpadded.is_empty()
        || !unpadded
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return None;
    }
    let bytes = URL_SAFE_NO_PAD.decode(unpadded).ok()?;
    (URL_SAFE_NO_PAD.encode(&bytes) == unpadded).then_some(bytes)
}

/// `validPushKeys`: `p256dh` is a valid uncompressed P-256 point, `auth` is 16 bytes.
#[must_use]
pub fn valid_keys(p256dh: &str, auth: &str) -> bool {
    let (Some(point), Some(auth)) = (base64url_bytes(p256dh), base64url_bytes(auth)) else {
        return false;
    };
    point.len() == 65
        && point[0] == 0x04
        && auth.len() == 16
        && p256::PublicKey::from_sec1_bytes(&point).is_ok()
}

/// Hex SHA-256 of the endpoint string, the subscription dedupe key.
#[must_use]
pub fn endpoint_sha256(endpoint: &str) -> String {
    hex::encode(Sha256::digest(endpoint.as_bytes()))
}

/// One-column form of the `WEBHOOK_ENC_KEY` record: `v1:<nonce>:<ciphertext>:<tag>` (standard
/// base64), shared with `packEndpointCiphertext` in `lib/push.js`.
#[must_use]
pub fn pack_endpoint_ciphertext(record: &EncryptedUrl) -> String {
    format!("v1:{}:{}:{}", record.nonce, record.ciphertext, record.tag)
}

/// Inverse of [`pack_endpoint_ciphertext`].
#[must_use]
pub fn unpack_endpoint_ciphertext(value: &str) -> Option<EncryptedUrl> {
    let mut parts = value.split(':');
    let (Some("v1"), Some(nonce), Some(ciphertext), Some(tag), None) = (
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
    ) else {
        return None;
    };
    let valid = |part: &str| BASE64_STANDARD.decode(part).is_ok();
    (valid(nonce) && valid(ciphertext) && valid(tag)).then(|| EncryptedUrl {
        ciphertext: ciphertext.to_owned(),
        nonce: nonce.to_owned(),
        tag: tag.to_owned(),
    })
}

/// Lowercased viewer email, the identity stored in every push table.
#[must_use]
pub fn normalize_email(value: &str) -> String {
    value.trim().to_lowercase()
}

// ---------------------------------------------------------------------------
// Subscriptions and opt-ins
// ---------------------------------------------------------------------------

/// A validated subscription write.
#[derive(Clone)]
pub struct NewSubscription<'a> {
    pub org: &'a str,
    pub viewer_email: &'a str,
    pub endpoint: &'a str,
    pub p256dh: &'a str,
    pub auth: &'a str,
    pub label: &'a str,
}

/// Upsert by endpoint hash. An existing row moves to this viewer and org with new keys; a new row
/// gets `new_id`. `endpoint_ciphertext` is [`pack_endpoint_ciphertext`] of the encrypted endpoint.
/// The viewer then keeps at most [`SUBSCRIPTIONS_PER_VIEWER`] rows, oldest first out.
///
/// # Errors
/// [`AppError::Internal`] when SQLite fails.
pub fn save_subscription(
    conn: &mut Connection,
    endpoint_ciphertext: &str,
    subscription: &NewSubscription<'_>,
    new_id: String,
    now_text: &str,
) -> Result<String, AppError> {
    let email = normalize_email(subscription.viewer_email);
    let sha = endpoint_sha256(subscription.endpoint);
    let ciphertext = endpoint_ciphertext;
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(db_error)?;
    let existing: Option<(String, String, String)> = tx
        .query_row(
            "SELECT id, viewer_email, org FROM push_subscriptions WHERE endpoint_sha256 = ?",
            [&sha],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()
        .map_err(db_error)?;
    let id = if let Some((id, previous_email, previous_org)) = existing {
        // Queued messages were addressed under the previous owner's eligibility.
        if previous_email != email || previous_org != subscription.org {
            tx.execute(
                "DELETE FROM push_deliveries WHERE subscription_id = ? AND state = 'pending'",
                [&id],
            )
            .map_err(db_error)?;
        }
        tx.execute(
            "UPDATE push_subscriptions SET org = ?, viewer_email = ?, endpoint_ciphertext = ?, p256dh = ?, auth = ?, label = ?, failure_count = 0 WHERE id = ?",
            params![subscription.org, email, ciphertext, subscription.p256dh, subscription.auth, subscription.label, id],
        )
        .map_err(db_error)?;
        id
    } else {
        tx.execute(
            "INSERT INTO push_subscriptions (id, org, viewer_email, endpoint_ciphertext, endpoint_sha256, p256dh, auth, label, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            params![new_id, subscription.org, email, ciphertext, sha, subscription.p256dh, subscription.auth, subscription.label, now_text],
        )
        .map_err(db_error)?;
        new_id
    };
    let others = {
        let mut stmt = tx
            .prepare("SELECT id FROM push_subscriptions WHERE viewer_email = ? ORDER BY created_at ASC, rowid ASC")
            .map_err(db_error)?;
        let rows = stmt
            .query_map([&email], |row| row.get::<_, String>(0))
            .map_err(db_error)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(db_error)?;
        rows.into_iter()
            .filter(|row| *row != id)
            .collect::<Vec<_>>()
    };
    let excess = others
        .len()
        .saturating_sub(SUBSCRIPTIONS_PER_VIEWER.saturating_sub(1));
    for old in others.iter().take(excess) {
        tx.execute("DELETE FROM push_subscriptions WHERE id = ?", [old])
            .map_err(db_error)?;
    }
    tx.commit().map_err(db_error)?;
    Ok(id)
}

/// Move every subscription of this viewer to their current organization, dropping pending
/// deliveries that were queued under the previous one. Returns the number of moved rows.
///
/// # Errors
/// [`AppError::Internal`] when SQLite fails.
pub fn refresh_subscription_org(
    conn: &mut Connection,
    viewer_email: &str,
    org: &str,
) -> Result<usize, AppError> {
    let email = normalize_email(viewer_email);
    let stale: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM push_subscriptions WHERE viewer_email = ? AND org <> ?",
            params![email, org],
            |row| row.get(0),
        )
        .map_err(db_error)?;
    if stale == 0 {
        return Ok(0);
    }
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(db_error)?;
    tx.execute(
        "DELETE FROM push_deliveries WHERE state = 'pending' AND subscription_id IN (SELECT id FROM push_subscriptions WHERE viewer_email = ? AND org <> ?)",
        params![email, org],
    )
    .map_err(db_error)?;
    let moved = tx
        .execute(
            "UPDATE push_subscriptions SET org = ? WHERE viewer_email = ? AND org <> ?",
            params![org, email, org],
        )
        .map_err(db_error)?;
    tx.commit().map_err(db_error)?;
    Ok(moved)
}

/// Remove the endpoint only when it belongs to this viewer.
///
/// # Errors
/// [`AppError::Internal`] when SQLite fails.
pub fn remove_subscription(
    conn: &Connection,
    viewer_email: &str,
    endpoint: &str,
) -> Result<bool, AppError> {
    conn.execute(
        "DELETE FROM push_subscriptions WHERE endpoint_sha256 = ? AND viewer_email = ?",
        params![endpoint_sha256(endpoint), normalize_email(viewer_email)],
    )
    .map(|changed| changed > 0)
    .map_err(db_error)
}

/// Subscriptions of this viewer that are eligible for an artifact in `org`: the same organization,
/// or the administrator tenant (administrators can read every organization).
///
/// # Errors
/// [`AppError::Internal`] when SQLite fails.
pub fn device_count(conn: &Connection, org: &str, viewer_email: &str) -> Result<i64, AppError> {
    conn.query_row(
        "SELECT COUNT(*) FROM push_subscriptions WHERE (org = ? OR org = 'admin') AND viewer_email = ?",
        params![org, normalize_email(viewer_email)],
        |row| row.get(0),
    )
    .map_err(db_error)
}

/// Whether the viewer opted in to this artifact.
///
/// # Errors
/// [`AppError::Internal`] when SQLite fails.
pub fn opted_in(
    conn: &Connection,
    artifact_id: &str,
    viewer_email: &str,
) -> Result<bool, AppError> {
    conn.query_row(
        "SELECT 1 FROM artifact_push_optins WHERE artifact_id = ? AND viewer_email = ?",
        params![artifact_id, normalize_email(viewer_email)],
        |_| Ok(()),
    )
    .optional()
    .map(|row| row.is_some())
    .map_err(db_error)
}

/// Record (or move) the per-artifact opt-in. `org` is the artifact's organization.
///
/// # Errors
/// [`AppError::Internal`] when SQLite fails.
pub fn opt_in(
    conn: &Connection,
    artifact_id: &str,
    org: &str,
    viewer_email: &str,
    now_text: &str,
) -> Result<(), AppError> {
    conn.execute(
        "INSERT INTO artifact_push_optins (artifact_id, viewer_email, org, created_at) VALUES (?, ?, ?, ?) ON CONFLICT(artifact_id, viewer_email) DO UPDATE SET org = excluded.org",
        params![artifact_id, normalize_email(viewer_email), org, now_text],
    )
    .map(|_| ())
    .map_err(db_error)
}

/// Remove the per-artifact opt-in. The device subscriptions stay.
///
/// # Errors
/// [`AppError::Internal`] when SQLite fails.
pub fn opt_out(conn: &Connection, artifact_id: &str, viewer_email: &str) -> Result<(), AppError> {
    conn.execute(
        "DELETE FROM artifact_push_optins WHERE artifact_id = ? AND viewer_email = ?",
        params![artifact_id, normalize_email(viewer_email)],
    )
    .map(|_| ())
    .map_err(db_error)
}

// ---------------------------------------------------------------------------
// Reminders
// ---------------------------------------------------------------------------

/// `owner` column: `''` for org scope, the lowercased email for viewer scope.
#[must_use]
pub fn reminder_owner(scope: ReminderScope, viewer_email: Option<&str>) -> String {
    match scope {
        ReminderScope::Org => String::new(),
        ReminderScope::Viewer => viewer_email.map(normalize_email).unwrap_or_default(),
    }
}

/// One armed reminder as listed to a viewer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ReminderRecord {
    pub key: String,
    pub scope: ReminderScope,
    pub fire_at: i64,
    pub title: String,
    pub body: String,
    pub revision: i64,
}

/// Armed rows in one `(artifact, scope, owner)` bucket, ordered by key.
///
/// # Errors
/// [`AppError::Internal`] when SQLite fails.
pub fn list_reminders(
    conn: &Connection,
    artifact_id: &str,
    scope: ReminderScope,
    owner: &str,
) -> Result<Vec<ReminderRecord>, AppError> {
    let mut stmt = conn
        .prepare("SELECT key, fire_at, title, body, revision FROM artifact_reminders WHERE artifact_id = ? AND scope = ? AND owner = ? AND state = 'armed' ORDER BY key")
        .map_err(db_error)?;
    stmt.query_map(params![artifact_id, scope.as_str(), owner], |row| {
        Ok(ReminderRecord {
            key: row.get(0)?,
            scope,
            fire_at: row.get(1)?,
            title: row.get(2)?,
            body: row.get(3)?,
            revision: row.get(4)?,
        })
    })
    .map_err(db_error)?
    .collect::<rusqlite::Result<Vec<_>>>()
    .map_err(db_error)
}

/// A validated reminder write.
#[derive(Clone, Debug)]
pub struct ReminderWrite<'a> {
    pub artifact_id: &'a str,
    /// The artifact's organization.
    pub org: &'a str,
    pub scope: ReminderScope,
    pub owner: &'a str,
    pub key: &'a str,
    pub fire_at: i64,
    pub text: &'a ReminderText,
    /// `viewer` or `publisher:<client_id>`; never an email.
    pub created_by: &'a str,
}

/// Arm or re-arm one reminder and return its new revision.
///
/// # Errors
/// [`PushError::ReminderLimit`] when a new armed row would exceed [`ARMED_REMINDER_LIMIT`];
/// [`PushError::BadScope`] for a viewer-scope write without an owner; otherwise
/// [`PushError::Unavailable`].
pub fn set_reminder(
    conn: &mut Connection,
    write: &ReminderWrite<'_>,
    now_text: &str,
) -> Result<i64, PushError> {
    if !valid_key(write.key) {
        return Err(PushError::BadKey);
    }
    if write.scope == ReminderScope::Viewer && write.owner.is_empty() {
        return Err(PushError::BadScope);
    }
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(db_error)?;
    let current: Option<(String, i64)> = tx
        .query_row(
            "SELECT state, revision FROM artifact_reminders WHERE artifact_id = ? AND scope = ? AND owner = ? AND key = ?",
            params![write.artifact_id, write.scope.as_str(), write.owner, write.key],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(db_error)?;
    // An armed key never counts as a new row; a new key, or a fired key being re-armed, does.
    if current.as_ref().is_none_or(|(state, _)| state != "armed") {
        let armed: i64 = tx
            .query_row(
                "SELECT COUNT(*) FROM artifact_reminders WHERE artifact_id = ? AND state = 'armed'",
                [write.artifact_id],
                |row| row.get(0),
            )
            .map_err(db_error)?;
        if armed >= ARMED_REMINDER_LIMIT {
            return Err(PushError::ReminderLimit);
        }
    }
    let revision = if let Some((_, revision)) = current {
        tx.execute(
            "UPDATE artifact_reminders SET org = ?, fire_at = ?, title = ?, body = ?, state = 'armed', revision = revision + 1, created_by = ?, updated_at = ?, fired_at = NULL WHERE artifact_id = ? AND scope = ? AND owner = ? AND key = ?",
            params![write.org, write.fire_at, write.text.title, write.text.body, write.created_by, now_text, write.artifact_id, write.scope.as_str(), write.owner, write.key],
        )
        .map_err(db_error)?;
        revision + 1
    } else {
        tx.execute(
            "INSERT INTO artifact_reminders (artifact_id, scope, owner, key, org, fire_at, title, body, state, revision, created_by, updated_at, fired_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, 'armed', 1, ?, ?, NULL)",
            params![write.artifact_id, write.scope.as_str(), write.owner, write.key, write.org, write.fire_at, write.text.title, write.text.body, write.created_by, now_text],
        )
        .map_err(db_error)?;
        1
    };
    tx.commit().map_err(db_error)?;
    Ok(revision)
}

/// Delete one reminder key. Absent keys are not an error.
///
/// # Errors
/// [`AppError::Internal`] when SQLite fails.
pub fn clear_reminder(
    conn: &Connection,
    artifact_id: &str,
    scope: ReminderScope,
    owner: &str,
    key: &str,
) -> Result<bool, AppError> {
    conn.execute(
        "DELETE FROM artifact_reminders WHERE artifact_id = ? AND scope = ? AND owner = ? AND key = ?",
        params![artifact_id, scope.as_str(), owner, key],
    )
    .map(|changed| changed > 0)
    .map_err(db_error)
}

// ---------------------------------------------------------------------------
// Sweeper
// ---------------------------------------------------------------------------

/// The notification payload. It never contains viewer emails.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct PushPayload {
    pub title: String,
    pub body: String,
    pub url: String,
    pub tag: String,
}

/// Aggregate result of one sweeper transaction.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SweepReport {
    /// Reminders fired with recipient expansion (possibly zero recipients).
    pub fired: u64,
    /// Reminders more than six hours late, marked fired without delivery.
    pub stale: u64,
    /// `push_deliveries` rows queued.
    pub deliveries: u64,
}

/// Org-scope recipients: every opted-in viewer's subscriptions whose organization is the artifact's
/// *current* organization, or the administrator tenant. Joining `artifacts` makes an artifact move
/// take effect immediately, whatever org the reminder or opt-in row recorded.
const ORG_RECIPIENTS_SQL: &str = "SELECT s.id FROM artifact_push_optins o JOIN artifacts a ON a.id = o.artifact_id JOIN push_subscriptions s ON s.viewer_email = o.viewer_email AND (s.org = a.org OR s.org = 'admin') WHERE o.artifact_id = ? ORDER BY s.created_at, s.rowid";
/// Viewer-scope recipients: the owner's opt-in (required) and the owner's eligible subscriptions.
const VIEWER_RECIPIENTS_SQL: &str = "SELECT s.id FROM artifact_push_optins o JOIN artifacts a ON a.id = o.artifact_id JOIN push_subscriptions s ON s.viewer_email = o.viewer_email AND (s.org = a.org OR s.org = 'admin') WHERE o.artifact_id = ? AND o.viewer_email = ? ORDER BY s.created_at, s.rowid";

/// Claim up to [`SWEEP_BATCH`] due reminders, expand each into one delivery per recipient
/// subscription, and mark them fired, all in one `BEGIN IMMEDIATE` transaction.
///
/// # Errors
/// [`AppError::Internal`] when SQLite or payload encoding fails; nothing is committed.
pub fn sweep_due(
    conn: &mut Connection,
    now_ms: i64,
    public_base_url: &str,
) -> Result<SweepReport, AppError> {
    let now_text = crate::config::format_sqlite_datetime(now_ms.div_euclid(1000));
    let base = public_base_url.trim_end_matches('/');
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(db_error)?;
    let due = {
        let mut stmt = tx
            .prepare("SELECT artifact_id, scope, owner, key, org, fire_at, title, body FROM artifact_reminders WHERE state = 'armed' AND fire_at <= ? ORDER BY fire_at, artifact_id, key LIMIT ?")
            .map_err(db_error)?;
        stmt.query_map(params![now_ms, SWEEP_BATCH], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, String>(7)?,
            ))
        })
        .map_err(db_error)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(db_error)?
    };
    let mut report = SweepReport::default();
    for (artifact_id, scope, owner, key, _recorded_org, fire_at, title, body) in due {
        if now_ms.saturating_sub(fire_at) <= STALE_AFTER_MS {
            let recipients = if scope == "viewer" {
                let mut stmt = tx.prepare_cached(VIEWER_RECIPIENTS_SQL).map_err(db_error)?;
                stmt.query_map(params![artifact_id, owner], |row| row.get::<_, String>(0))
                    .map_err(db_error)?
                    .collect::<rusqlite::Result<Vec<_>>>()
                    .map_err(db_error)?
            } else {
                let mut stmt = tx.prepare_cached(ORG_RECIPIENTS_SQL).map_err(db_error)?;
                stmt.query_map([&artifact_id], |row| row.get::<_, String>(0))
                    .map_err(db_error)?
                    .collect::<rusqlite::Result<Vec<_>>>()
                    .map_err(db_error)?
            };
            let payload = serde_json::to_string(&PushPayload {
                title,
                body,
                url: format!("{base}/{artifact_id}"),
                tag: format!("{artifact_id}:{key}"),
            })
            .map_err(|_| AppError::Internal)?;
            for subscription in &recipients {
                tx.execute(
                    "INSERT INTO push_deliveries (subscription_id, artifact_id, reminder_key, payload, state, attempts, next_attempt_at, expires_at, created_at, updated_at) VALUES (?, ?, ?, ?, 'pending', 0, ?, ?, ?, ?)",
                    params![subscription, artifact_id, key, payload, now_ms, now_ms + DELIVERY_TTL_MS, now_text, now_text],
                )
                .map_err(db_error)?;
            }
            report.fired += 1;
            report.deliveries += recipients.len() as u64;
        } else {
            report.stale += 1;
        }
        tx.execute(
            "UPDATE artifact_reminders SET state = 'fired', fired_at = ? WHERE artifact_id = ? AND scope = ? AND owner = ? AND key = ?",
            params![now_text, artifact_id, scope, owner, key],
        )
        .map_err(db_error)?;
    }
    tx.commit().map_err(db_error)?;
    Ok(report)
}

// ---------------------------------------------------------------------------
// Sender
// ---------------------------------------------------------------------------

/// A pending delivery joined to its subscription.
#[derive(Clone)]
pub struct DueDelivery {
    pub id: i64,
    pub subscription_id: String,
    pub artifact_id: String,
    pub reminder_key: String,
    pub payload: String,
    pub attempts: i64,
    pub expires_at: i64,
    pub endpoint_ciphertext: String,
    pub p256dh: String,
    pub auth: String,
}

impl std::fmt::Debug for DueDelivery {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DueDelivery")
            .field("id", &self.id)
            .field("attempts", &self.attempts)
            .finish_non_exhaustive()
    }
}

/// Mark pending deliveries past `expires_at` as dead; returns how many.
///
/// # Errors
/// [`AppError::Internal`] when SQLite fails.
pub fn expire_overdue(conn: &Connection, now_ms: i64) -> Result<u64, AppError> {
    let now_text = crate::config::format_sqlite_datetime(now_ms.div_euclid(1000));
    conn.execute(
        "UPDATE push_deliveries SET state = 'dead', updated_at = ? WHERE state = 'pending' AND expires_at <= ?",
        params![now_text, now_ms],
    )
    .map(|changed| changed as u64)
    .map_err(db_error)
}

/// Lease length for a claimed delivery: the 10-second send timeout plus a 20-second margin.
pub const DELIVERY_LEASE_MS: i64 = 30_000;
/// Finished deliveries are kept this long for operator review.
pub const FINISHED_RETENTION_MS: i64 = 7 * 24 * 60 * 60 * 1000;

/// Claim due pending deliveries: in one `BEGIN IMMEDIATE` transaction, select rows with
/// `next_attempt_at <= now` and push their `next_attempt_at` to `now + lease_ms`, so a concurrent
/// claim cannot take the same rows while they are in flight. A crashed sender's rows become due
/// again when the lease ends (at-least-once).
///
/// # Errors
/// [`AppError::Internal`] when SQLite fails.
pub fn claim_due(
    conn: &mut Connection,
    now_ms: i64,
    limit: i64,
    lease_ms: i64,
) -> Result<Vec<DueDelivery>, AppError> {
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(db_error)?;
    let claimed = due_deliveries(&tx, now_ms, limit)?;
    for delivery in &claimed {
        tx.execute(
            "UPDATE push_deliveries SET next_attempt_at = ? WHERE id = ? AND state = 'pending'",
            params![now_ms + lease_ms, delivery.id],
        )
        .map_err(db_error)?;
    }
    tx.commit().map_err(db_error)?;
    Ok(claimed)
}

/// Delete `accepted` and `dead` deliveries last updated more than seven days ago.
///
/// # Errors
/// [`AppError::Internal`] when SQLite fails.
pub fn prune_finished(conn: &Connection, now_ms: i64) -> Result<u64, AppError> {
    let cutoff =
        crate::config::format_sqlite_datetime((now_ms - FINISHED_RETENTION_MS).div_euclid(1000));
    conn.execute(
        "DELETE FROM push_deliveries WHERE state IN ('accepted','dead') AND updated_at < ?",
        [cutoff],
    )
    .map(|changed| changed as u64)
    .map_err(db_error)
}

/// Pending deliveries whose `next_attempt_at` has passed, oldest first (no lease).
///
/// # Errors
/// [`AppError::Internal`] when SQLite fails.
pub fn due_deliveries(
    conn: &Connection,
    now_ms: i64,
    limit: i64,
) -> Result<Vec<DueDelivery>, AppError> {
    let mut stmt = conn
        .prepare("SELECT d.id, d.subscription_id, d.artifact_id, d.reminder_key, d.payload, d.attempts, d.expires_at, s.endpoint_ciphertext, s.p256dh, s.auth FROM push_deliveries d JOIN push_subscriptions s ON s.id = d.subscription_id WHERE d.state = 'pending' AND d.next_attempt_at <= ? ORDER BY d.next_attempt_at, d.id LIMIT ?")
        .map_err(db_error)?;
    stmt.query_map(params![now_ms, limit], |row| {
        Ok(DueDelivery {
            id: row.get(0)?,
            subscription_id: row.get(1)?,
            artifact_id: row.get(2)?,
            reminder_key: row.get(3)?,
            payload: row.get(4)?,
            attempts: row.get(5)?,
            expires_at: row.get(6)?,
            endpoint_ciphertext: row.get(7)?,
            p256dh: row.get(8)?,
            auth: row.get(9)?,
        })
    })
    .map_err(db_error)?
    .collect::<rusqlite::Result<Vec<_>>>()
    .map_err(db_error)
}

/// `200`/`201`/`202`: the push service accepted the message.
///
/// # Errors
/// [`AppError::Internal`] when SQLite fails.
pub fn mark_accepted(
    conn: &mut Connection,
    delivery: &DueDelivery,
    status: u16,
    now_ms: i64,
) -> Result<(), AppError> {
    let now_text = crate::config::format_sqlite_datetime(now_ms.div_euclid(1000));
    let tx = conn.transaction().map_err(db_error)?;
    tx.execute(
        "UPDATE push_deliveries SET state = 'accepted', attempts = attempts + 1, last_status = ?, updated_at = ? WHERE id = ?",
        params![status, now_text, delivery.id],
    )
    .map_err(db_error)?;
    tx.execute(
        "UPDATE push_subscriptions SET last_success_at = ?, failure_count = 0 WHERE id = ?",
        params![now_text, delivery.subscription_id],
    )
    .map_err(db_error)?;
    tx.commit().map_err(db_error)
}

/// `404`/`410`: delete the subscription; its deliveries cascade away.
///
/// # Errors
/// [`AppError::Internal`] when SQLite fails.
pub fn remove_subscription_by_id(
    conn: &mut Connection,
    subscription_id: &str,
) -> Result<bool, AppError> {
    let tx = conn.transaction().map_err(db_error)?;
    // Explicit, so the outcome does not depend on the connection's foreign-key pragma.
    tx.execute(
        "DELETE FROM push_deliveries WHERE subscription_id = ?",
        [subscription_id],
    )
    .map_err(db_error)?;
    let removed = tx
        .execute(
            "DELETE FROM push_subscriptions WHERE id = ?",
            [subscription_id],
        )
        .map_err(db_error)?;
    tx.commit().map_err(db_error)?;
    Ok(removed > 0)
}

/// Whether a retryable failure was rescheduled or ran out of time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RetryOutcome {
    Retrying,
    Dead,
}

/// `429`, `5xx`, network errors: count the failure and reschedule, or mark the delivery dead when
/// the next attempt would fall at or past `expires_at`.
///
/// # Errors
/// [`AppError::Internal`] when SQLite fails.
pub fn mark_retry(
    conn: &mut Connection,
    delivery: &DueDelivery,
    status: Option<u16>,
    next_attempt_at: i64,
    now_ms: i64,
) -> Result<RetryOutcome, AppError> {
    let now_text = crate::config::format_sqlite_datetime(now_ms.div_euclid(1000));
    let outcome = if next_attempt_at >= delivery.expires_at {
        RetryOutcome::Dead
    } else {
        RetryOutcome::Retrying
    };
    let tx = conn.transaction().map_err(db_error)?;
    tx.execute(
        "UPDATE push_deliveries SET state = ?, attempts = attempts + 1, next_attempt_at = ?, last_status = ?, updated_at = ? WHERE id = ?",
        params![
            if outcome == RetryOutcome::Dead { "dead" } else { "pending" },
            next_attempt_at,
            status,
            now_text,
            delivery.id
        ],
    )
    .map_err(db_error)?;
    tx.execute(
        "UPDATE push_subscriptions SET failure_count = failure_count + 1 WHERE id = ?",
        [&delivery.subscription_id],
    )
    .map_err(db_error)?;
    tx.commit().map_err(db_error)?;
    Ok(outcome)
}

/// Any other status, or an endpoint that no longer passes the allowlist: give up.
///
/// # Errors
/// [`AppError::Internal`] when SQLite fails.
pub fn mark_dead(
    conn: &Connection,
    delivery: &DueDelivery,
    status: Option<u16>,
    now_ms: i64,
) -> Result<(), AppError> {
    let now_text = crate::config::format_sqlite_datetime(now_ms.div_euclid(1000));
    conn.execute(
        "UPDATE push_deliveries SET state = 'dead', attempts = attempts + 1, last_status = ?, updated_at = ? WHERE id = ?",
        params![status, now_text, delivery.id],
    )
    .map(|_| ())
    .map_err(db_error)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ciphertext_packing_round_trips_and_rejects_other_shapes() {
        let record = EncryptedUrl {
            ciphertext: "YWJj".into(),
            nonce: "AAAAAAAAAAAAAAAA".into(),
            tag: "AAAAAAAAAAAAAAAAAAAAAA==".into(),
        };
        let packed = pack_endpoint_ciphertext(&record);
        assert_eq!(packed, "v1:AAAAAAAAAAAAAAAA:YWJj:AAAAAAAAAAAAAAAAAAAAAA==");
        assert_eq!(unpack_endpoint_ciphertext(&packed), Some(record));
        assert_eq!(unpack_endpoint_ciphertext("v2:a:b:c"), None);
        assert_eq!(unpack_endpoint_ciphertext("v1:a:b"), None);
    }

    #[test]
    fn host_rules_match_exact_names_and_subdomain_wildcards() {
        let hosts = vec![
            "fcm.googleapis.com".to_owned(),
            "*.push.apple.com".to_owned(),
        ];
        assert!(host_allowed("fcm.googleapis.com", &hosts));
        assert!(host_allowed("api.push.apple.com", &hosts));
        assert!(!host_allowed("push.apple.com", &hosts));
        assert!(!host_allowed("evilpush.apple.com", &hosts));
        assert!(!host_allowed("x.fcm.googleapis.com", &hosts));
    }
}
