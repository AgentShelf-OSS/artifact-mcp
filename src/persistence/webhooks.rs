//! Owned by U12 (terra) — encrypted webhook persistence.
//!
//! Port of `lib/webhooks.js`. This module and [`crate::integrations::notify`] are the only two
//! places a full Discord webhook URL is ever materialised; everything else sees the masked form.
//!
//! # At-rest layout (authority: `lib/webhooks.js:14-21`, migration v18)
//!
//! | Column | With a key | Without a key |
//! |---|---|---|
//! | `url` | masked display value (`storedMaskUrl`) | the URL **verbatim** |
//! | `url_cipher`/`url_nonce`/`url_tag` | AES-256-GCM record | `NULL` |
//!
//! `WEBHOOK_ENC_KEY` is currently unset in production, so the plaintext column is a live
//! configuration and not a legacy path. Both are modelled by U04's
//! [`WebhookUrlProtection`], which this module consumes rather than reimplementing.
//!
//! # Two different masks
//!
//! `lib/webhooks.js` has *two* maskers and they differ by one character:
//!
//! * `storedMaskUrl` — `{scheme}://{host}/…{last4}` — what is written to the `url` column when a
//!   row is encrypted. Already ported as
//!   [`mask_webhook_url`](crate::persistence::migrations::mask_webhook_url) (U03) and applied by
//!   [`WebhookUrlProtection::protect`] (U04).
//! * `maskUrl` — `{scheme}://{host}…{last4}` (no slash) — what [`WebhookRow::public`] shows. It is
//!   applied to whatever is in the `url` column, so an encrypted row is masked *twice*: the stored
//!   `https://discord.com/…oken` renders as `https://discord.com…oken`. That is the Node
//!   behaviour and the admin UI depends on it, so it is reproduced exactly.
//!
//! # Secret containment
//!
//! * [`WebhookRow`] and [`WebhookStore`] have hand-written `Debug` impls that redact the URL, so
//!   no accidental `{:?}` can print a token.
//! * No error returned from this module embeds a URL: the reveal failure path collapses to
//!   [`AppError::Internal`] inside U04's cipher.
//! * [`WebhookSummary`] — the only shape that reaches an HTTP response — always carries the masked
//!   value.

use std::sync::Arc;

use rusqlite::{Connection, OptionalExtension, Row, TransactionBehavior};

use crate::security::audit::{
    AuditEvent, MutationAudit, append_in_transaction, mutate_in_transaction,
};
use crate::{
    config::IdSource,
    error::AppError,
    model::{
        CreateWebhook, OrgId, Timestamp, WebhookDelivery, WebhookEvent, WebhookId, WebhookSummary,
    },
    persistence::{
        db::{self, DbPool},
        migrations::EncryptedUrl,
    },
    security::crypto::{StoredWebhookUrl, WebhookUrlProtection},
};

/// Every webhook event, in the frozen `lib/webhooks.js:9` order.
pub const EVENTS: [WebhookEvent; 6] = [
    WebhookEvent::Published,
    WebhookEvent::Updated,
    WebhookEvent::Restored,
    WebhookEvent::Deleted,
    WebhookEvent::Feedback,
    WebhookEvent::Resolved,
];

/// Verbatim Node rejection for a URL outside the Discord allowlist. [lib/webhooks.js:87]
pub const INVALID_URL_MESSAGE: &str = "Webhook URL must be an HTTPS Discord webhook URL.";

/// `String(label).trim().slice(0, 80)` — [lib/webhooks.js:97].
pub const MAX_LABEL_UTF16: usize = 80;

/// `String(error).slice(0, 500)` — [lib/webhooks.js:128].
pub const MAX_ERROR_UTF16: usize = 500;

/// Default recorded failure text. [lib/webhooks.js:128]
pub const DEFAULT_DELIVERY_ERROR: &str = "Webhook delivery failed.";

/// A deliberately data-free result for the durable worker's just-in-time webhook lookup.
///
/// This API is narrower than [`WebhookStore::delivery`]: it binds the row to the outbox tenant
/// before materialising its URL and keeps database availability separate from an invalid target
/// and an authenticated-ciphertext failure.  The variants contain neither a webhook URL nor
/// cipher/key material, so they are safe to put in worker diagnostics.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WebhookDeliveryResolutionFailure {
    /// The store could not complete its database operation. The outbox item may succeed later.
    Retryable,
    /// The reference is blank, missing, or belongs to another tenant.
    InvalidReference,
    /// The encrypted row could not be authenticated or its configured key is unavailable.
    DecryptFailed,
}

/// Column list shared by every read; `SELECT *` is avoided so a later migration cannot silently
/// change the row shape underneath the mapper.
const ROW_COLUMNS: &str = "id, org, url, url_cipher, url_nonce, url_tag, label, events, \
                           created_at, last_ok_at, last_error";

/// Frozen ordering from `listForOrgStmt`. [lib/webhooks.js:12]
const ORDER_BY: &str = "ORDER BY created_at ASC, id ASC";

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

/// The wire/column name of an event, matching `EVENTS` in `lib/webhooks.js:9`.
#[must_use]
pub const fn event_name(event: &WebhookEvent) -> &'static str {
    match *event {
        WebhookEvent::Published => "published",
        WebhookEvent::Updated => "updated",
        WebhookEvent::Restored => "restored",
        WebhookEvent::Deleted => "deleted",
        WebhookEvent::Feedback => "feedback",
        WebhookEvent::Resolved => "resolved",
    }
}

/// Inverse of [`event_name`]; unknown names are dropped, exactly as `rowEvents` filters them.
/// [lib/webhooks.js:35-37]
#[must_use]
pub fn event_from_name(value: &str) -> Option<WebhookEvent> {
    match value {
        "published" => Some(WebhookEvent::Published),
        "updated" => Some(WebhookEvent::Updated),
        "restored" => Some(WebhookEvent::Restored),
        "deleted" => Some(WebhookEvent::Deleted),
        "feedback" => Some(WebhookEvent::Feedback),
        "resolved" => Some(WebhookEvent::Resolved),
        _ => None,
    }
}

/// Port of `rowEvents(row)` — split on `,` and drop anything not in `EVENTS`.
/// [lib/webhooks.js:35-37]
#[must_use]
pub fn parse_stored_events(column: &str) -> Vec<WebhookEvent> {
    column.split(',').filter_map(event_from_name).collect()
}

/// Port of `parseEvents(value, { defaultAll })` — [lib/webhooks.js:25-33].
///
/// The typed model removes two of Node's four outcomes by construction: a non-array and an unknown
/// event name cannot be represented by `Option<&[WebhookEvent]>`, so the
/// `"Webhook events must be an array."` and `"Unknown webhook event: …"` throws are unreachable
/// here. What remains — the `defaultAll` behaviour and order-preserving de-duplication — is
/// reproduced exactly.
#[must_use]
pub fn normalize_events(events: Option<&[WebhookEvent]>, default_all: bool) -> Vec<WebhookEvent> {
    let Some(requested) = events else {
        return if default_all {
            EVENTS.to_vec()
        } else {
            Vec::new()
        };
    };
    let mut unique: Vec<WebhookEvent> = Vec::with_capacity(requested.len());
    for event in requested {
        if !unique.contains(event) {
            unique.push(event.clone());
        }
    }
    unique
}

/// The `events` column value for a normalized set.
#[must_use]
pub fn encode_events(events: &[WebhookEvent]) -> String {
    events
        .iter()
        .map(event_name)
        .collect::<Vec<&str>>()
        .join(",")
}

// ---------------------------------------------------------------------------
// URL masking and the Discord allowlist
// ---------------------------------------------------------------------------

/// Port of the exported `maskUrl(value)` — [lib/webhooks.js:39-47].
///
/// `${parsed.protocol}//${parsed.host}…${url.slice(-4)}`, falling back to `…${url.slice(-4)}` when
/// the value does not parse as a URL. Note the missing `/` relative to
/// [`mask_webhook_url`](crate::persistence::migrations::mask_webhook_url); the difference is in the
/// reference and is load-bearing for the admin UI's rendered value.
#[must_use]
pub fn mask_url(value: &str) -> String {
    let suffix = last_four_utf16(value);
    url::Url::parse(value).map_or_else(
        |_| format!("…{suffix}"),
        |parsed| format!("{}://{}…{suffix}", parsed.scheme(), js_host(&parsed)),
    )
}

/// JavaScript `URL.host`: hostname plus `:port` only when the port is not the scheme default.
///
/// `url::Url::port()` already returns `None` for a default port, so the two agree.
fn js_host(parsed: &url::Url) -> String {
    let host = parsed.host_str().unwrap_or_default();
    parsed
        .port()
        .map_or_else(|| host.to_owned(), |port| format!("{host}:{port}"))
}

/// Equivalent of JavaScript `value.slice(-4)`, which counts UTF-16 code units.
///
/// Splitting a surrogate pair is impossible in Rust, so a value whose 4th-from-last code unit is
/// the low half of an astral character yields the whole character instead of a lone surrogate.
/// Every value reaching this function is a URL or an already-masked URL, both ASCII in practice.
fn last_four_utf16(value: &str) -> String {
    if utf16_len(value) <= 4 {
        return value.to_owned();
    }
    let mut taken = 0;
    let mut start = value.len();
    for (index, character) in value.char_indices().rev() {
        taken += character.len_utf16();
        start = index;
        if taken >= 4 {
            break;
        }
    }
    value[start..].to_owned()
}

/// Number of UTF-16 code units in a string, i.e. JavaScript's `String.prototype.length`.
#[must_use]
pub fn utf16_len(value: &str) -> usize {
    value.chars().map(char::len_utf16).sum()
}

/// Equivalent of JavaScript `value.slice(0, max)` over UTF-16 code units.
///
/// As with [`last_four_utf16`], a cut that would land inside a surrogate pair keeps the whole
/// character rather than producing an unrepresentable lone surrogate.
#[must_use]
pub fn truncate_utf16(value: &str, max: usize) -> String {
    let mut taken = 0;
    for (index, character) in value.char_indices() {
        let width = character.len_utf16();
        if taken + width > max {
            return value[..index].to_owned();
        }
        taken += width;
    }
    value.to_owned()
}

/// Port of `DISCORD_WEBHOOK_RE.test(url)` — [lib/webhooks.js:23,87].
///
/// The regex is `^https://(discord|discordapp)\.com/api/webhooks/` with the `i` flag, anchored at
/// the start only. It is a **prefix allowlist on the raw string**, which is what makes lookalikes
/// such as `https://discord.com.evil.tld/api/webhooks/1/t` fail: the character after `discord.com`
/// must be `/`.
///
/// This is the SSRF guard. It is deliberately not re-expressed as "parse the URL, then compare the
/// host", because a parser difference between the two runtimes would then become a *security*
/// difference; matching the reference's literal prefix test keeps the two decisions identical.
#[must_use]
pub fn is_discord_webhook_url(value: &str) -> bool {
    const PREFIXES: [&str; 2] = [
        "https://discord.com/api/webhooks/",
        "https://discordapp.com/api/webhooks/",
    ];
    // `eq_ignore_ascii_case` over a prefix slice reproduces the regex's `i` flag, which is
    // ASCII-only for these literals.
    PREFIXES.iter().any(|prefix| {
        value
            .as_bytes()
            .get(..prefix.len())
            .is_some_and(|head| head.eq_ignore_ascii_case(prefix.as_bytes()))
    })
}

// ---------------------------------------------------------------------------
// Rows
// ---------------------------------------------------------------------------

/// One `org_webhooks` row.
///
/// `url` is whatever the column holds — the masked display value for an encrypted row, the real
/// URL for a plaintext one — so this type is treated as secret-bearing throughout.
#[derive(Clone, PartialEq, Eq)]
pub struct WebhookRow {
    /// Row identifier.
    pub id: WebhookId,
    /// Owning organization.
    pub org: OrgId,
    url: String,
    encrypted: Option<EncryptedUrl>,
    /// Operator-supplied label, already truncated at write time.
    pub label: String,
    /// Subscribed events, unknown names dropped.
    pub events: Vec<WebhookEvent>,
    /// `created_at` column.
    pub created_at: Timestamp,
    /// Last successful delivery.
    pub last_ok_at: Option<Timestamp>,
    /// Last recorded delivery failure.
    pub last_error: Option<String>,
}

impl WebhookRow {
    /// The three at-rest URL columns as U04's value type.
    #[must_use]
    pub fn stored_url(&self) -> StoredWebhookUrl {
        StoredWebhookUrl {
            url: self.url.clone(),
            encrypted: self.encrypted.clone(),
        }
    }

    /// `true` when this row's URL is encrypted at rest.
    #[must_use]
    pub const fn is_encrypted(&self) -> bool {
        self.encrypted.is_some()
    }

    /// Port of `publicRow(row)` — the only webhook shape allowed into an HTTP response.
    /// [lib/webhooks.js:59-69]
    #[must_use]
    pub fn public(&self) -> WebhookSummary {
        WebhookSummary {
            id: self.id.clone(),
            label: self.label.clone(),
            events: self.events.clone(),
            url: mask_url(&self.url),
            last_ok_at: self.last_ok_at.clone(),
            last_error: self.last_error.clone(),
        }
    }

    /// Port of `deliveryRow(row)` — [lib/webhooks.js:121-124].
    ///
    /// A row with no ciphertext is returned as-is (the live plaintext configuration). A row *with*
    /// ciphertext requires the key: with no key configured this fails closed rather than handing
    /// back the masked column as if it were a URL.
    ///
    /// # Errors
    /// Returns [`AppError::Internal`] when the row is encrypted and the key is missing or the
    /// record does not authenticate. The error carries no row content.
    pub fn delivery(&self, protection: &WebhookUrlProtection) -> Result<WebhookDelivery, AppError> {
        Ok(WebhookDelivery {
            id: self.id.clone(),
            org: self.org.clone(),
            url: protection.reveal(&self.stored_url())?,
            label: self.label.clone(),
            events: self.events.clone(),
        })
    }

    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        let cipher: Option<String> = row.get("url_cipher")?;
        let nonce: Option<String> = row.get("url_nonce")?;
        let tag: Option<String> = row.get("url_tag")?;
        // Node keys the encrypted branch on `url_cipher` alone (`if (!row?.url_cipher) return row`),
        // so a row missing a sibling column is still treated as encrypted and fails to decrypt
        // rather than silently falling back to the masked column.
        let encrypted = cipher.map(|ciphertext| EncryptedUrl {
            ciphertext,
            nonce: nonce.unwrap_or_default(),
            tag: tag.unwrap_or_default(),
        });
        Ok(Self {
            id: WebhookId(row.get("id")?),
            org: OrgId(row.get("org")?),
            url: row.get("url")?,
            encrypted,
            label: row.get("label")?,
            events: parse_stored_events(&row.get::<_, String>("events")?),
            created_at: Timestamp(row.get("created_at")?),
            last_ok_at: row.get::<_, Option<String>>("last_ok_at")?.map(Timestamp),
            last_error: row.get("last_error")?,
        })
    }
}

/// Redacted by construction: the `url` column can hold a live secret in the plaintext
/// configuration, so it never reaches a formatter.
impl std::fmt::Debug for WebhookRow {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WebhookRow")
            .field("id", &self.id)
            .field("org", &self.org)
            .field("url", &"<redacted>")
            .field("encrypted", &self.encrypted.is_some())
            .field("label", &self.label)
            .field("events", &self.events)
            .field("created_at", &self.created_at)
            .field("last_ok_at", &self.last_ok_at)
            .field("last_error", &self.last_error)
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Store
// ---------------------------------------------------------------------------

/// Per-org Discord webhook persistence on U03's pool.
#[derive(Clone)]
pub struct WebhookStore {
    pool: DbPool,
    ids: Arc<dyn IdSource>,
    protection: Arc<WebhookUrlProtection>,
}

impl WebhookStore {
    /// Bind the store to a pool, an id factory, and the at-rest protection mode.
    #[must_use]
    pub const fn new(
        pool: DbPool,
        ids: Arc<dyn IdSource>,
        protection: Arc<WebhookUrlProtection>,
    ) -> Self {
        Self {
            pool,
            ids,
            protection,
        }
    }

    /// The configured at-rest protection, shared with the notifier.
    #[must_use]
    pub const fn protection(&self) -> &Arc<WebhookUrlProtection> {
        &self.protection
    }

    /// Port of `listForOrg(org)` — masked rows only. [lib/webhooks.js:71-73]
    ///
    /// # Errors
    /// Returns [`AppError::Unavailable`] or [`AppError::Internal`] on a database fault.
    pub async fn list_for_org(&self, org: &OrgId) -> Result<Vec<WebhookSummary>, AppError> {
        let org = trimmed(&org.0);
        let rows = db::interact(&self.pool, move |conn| rows_for_org(conn, &org)).await?;
        Ok(rows.iter().map(WebhookRow::public).collect())
    }

    /// Port of `forEvent(org, event)` — internal, returns **decrypted** URLs.
    /// [lib/webhooks.js:76-81]
    ///
    /// Never hand the result to a renderer or an HTTP response.
    ///
    /// # Errors
    /// Propagates a database fault, or [`AppError::Internal`] when an encrypted row cannot be
    /// revealed with the configured key.
    pub async fn for_event(
        &self,
        org: &OrgId,
        event: &WebhookEvent,
    ) -> Result<Vec<WebhookDelivery>, AppError> {
        let org = trimmed(&org.0);
        let rows = db::interact(&self.pool, move |conn| rows_for_org(conn, &org)).await?;
        rows.iter()
            .filter(|row| row.events.contains(event))
            .map(|row| row.delivery(&self.protection))
            .collect()
    }

    /// Port of `create({ org, url, label, events })` — [lib/webhooks.js:83-102].
    ///
    /// With a key the row stores the masked URL plus the three cipher columns; without one the URL
    /// is stored verbatim with NULL cipher columns. Both are handled by
    /// [`WebhookUrlProtection::protect`].
    ///
    /// # Errors
    /// Returns [`AppError::Validation`] for an unknown organization or a URL outside the Discord
    /// allowlist, propagating the Node message verbatim; otherwise a database or cipher fault.
    pub async fn create(&self, request: CreateWebhook) -> Result<WebhookSummary, AppError> {
        let org = trimmed(&request.org.0);
        let url = trimmed(&request.url);
        let url_is_allowed = is_discord_webhook_url(&url);
        let id = self.ids.webhook_id()?;
        let label = truncate_utf16(&trimmed(&request.label), MAX_LABEL_UTF16);
        let events = encode_events(&normalize_events(request.events.as_deref(), true));
        // Encrypting before the org check costs one AEAD call on a rejected request and keeps the
        // secret out of the blocking closure's captured state as a bare `String`.
        let stored = self.protection.protect(&url)?;

        db::interact(&self.pool, move |conn| {
            create_in(
                conn,
                &org,
                &id,
                &url,
                url_is_allowed,
                &label,
                &events,
                &stored,
            )
        })
        .await
    }

    pub async fn create_audited(
        &self,
        request: CreateWebhook,
        audit: MutationAudit,
        audit_key: [u8; 32],
    ) -> Result<WebhookSummary, AppError> {
        let org = trimmed(&request.org.0);
        let url = trimmed(&request.url);
        let label = truncate_utf16(&trimmed(&request.label), MAX_LABEL_UTF16);
        let events = encode_events(&normalize_events(request.events.as_deref(), true));
        let url_is_allowed = is_discord_webhook_url(&url);
        let id = self.ids.webhook_id()?;
        let stored = self.protection.protect(&url)?;
        let target = org.clone();
        db::interact(&self.pool, move |conn| {
            let audit = audit.for_target_tenant(&org)?;
            mutate_in_transaction(conn, &audit_key, &audit, |tx| {
                let row = create_in(
                    tx,
                    &org,
                    &id,
                    &url,
                    url_is_allowed,
                    &label,
                    &events,
                    &stored,
                )?;
                Ok((
                    row,
                    AuditEvent {
                        operation: "webhook.config.create".to_owned(),
                        target_type: "organization".to_owned(),
                        target_id: target,
                        result: "success".to_owned(),
                        classification: "webhook_config_created".to_owned(),
                        revision: None,
                    },
                ))
            })
        })
        .await
    }

    pub async fn remove_audited(
        &self,
        org: OrgId,
        id: WebhookId,
        audit: MutationAudit,
        audit_key: [u8; 32],
    ) -> Result<bool, AppError> {
        let org = trimmed(&org.0);
        let id = trimmed(&id.0);
        let target = org.clone();
        db::interact(&self.pool, move |conn| {
            let audit = audit.for_target_tenant(&org)?;
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(|_| AppError::Internal)?;
            if discussion_anchor_exists(&tx, &org, &id)? {
                return Err(AppError::Conflict(
                    "Remove the Discord notification-thread connection before deleting this webhook."
                        .to_owned(),
                ));
            }
            let changed = tx
                .execute(
                    "DELETE FROM org_webhooks WHERE org = ?1 AND id = ?2",
                    (&org, &id),
                )
                .map_err(internal)?
                > 0;
            if !changed {
                tx.commit().map_err(|_| AppError::Internal)?;
                return Ok(false);
            }
            let event = AuditEvent {
                operation: "webhook.config.remove".to_owned(),
                target_type: "organization".to_owned(),
                target_id: target,
                result: "success".to_owned(),
                classification: "webhook_config_removed".to_owned(),
                revision: None,
            };
            append_in_transaction(&tx, &audit_key, &audit.event_id()?, audit.context(), &event)?;
            tx.commit().map_err(|_| AppError::Internal)?;
            Ok(true)
        })
        .await
    }

    pub async fn set_events_audited(
        &self,
        org: OrgId,
        id: WebhookId,
        events: Vec<WebhookEvent>,
        audit: MutationAudit,
        audit_key: [u8; 32],
    ) -> Result<Option<WebhookSummary>, AppError> {
        let normalized = encode_events(&normalize_events(Some(&events), false));
        let org = trimmed(&org.0);
        let id = trimmed(&id.0);
        let target = org.clone();
        db::interact(&self.pool, move |conn| {
            let audit = audit.for_target_tenant(&org)?;
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(|_| AppError::Internal)?;
            let Some(current) = row_by_id(&tx, &id)?.filter(|row| row.org.0 == org) else {
                tx.commit().map_err(|_| AppError::Internal)?;
                return Ok(None);
            };
            if !normalized.split(',').any(|event| event == "published")
                && discussion_anchor_exists(&tx, &org, &id)?
            {
                return Err(AppError::Conflict(
                    "Published notifications are required by the Discord thread connection."
                        .to_owned(),
                ));
            }
            if encode_events(&current.events) == normalized {
                let row = current.public();
                tx.commit().map_err(|_| AppError::Internal)?;
                return Ok(Some(row));
            }
            tx.execute(
                "UPDATE org_webhooks SET events = ?1 WHERE org = ?2 AND id = ?3",
                (&normalized, &org, &id),
            )
            .map_err(internal)?;
            let row = row_by_id(&tx, &id)?
                .map(|row| row.public())
                .ok_or(AppError::Internal)?;
            let event = AuditEvent {
                operation: "webhook.config.update".to_owned(),
                target_type: "organization".to_owned(),
                target_id: target,
                result: "success".to_owned(),
                classification: "webhook_config_updated".to_owned(),
                revision: None,
            };
            append_in_transaction(&tx, &audit_key, &audit.event_id()?, audit.context(), &event)?;
            tx.commit().map_err(|_| AppError::Internal)?;
            Ok(Some(row))
        })
        .await
    }

    /// Persist one half of the explicitly non-atomic webhook-test protocol. `None` records the
    /// durable pre-I/O request marker; `Some` records the terminal external delivery result.
    /// The persisted target is only `(tenant org, webhook id)`; the decrypted URL never crosses
    /// this boundary. A crash after delivery can therefore leave a visible requested-only record
    /// rather than silently losing evidence of the attempt.
    pub async fn audit_test(
        &self,
        org: OrgId,
        id: WebhookId,
        outcome: Option<bool>,
        audit: MutationAudit,
        audit_key: [u8; 32],
    ) -> Result<(), AppError> {
        let org = trimmed(&org.0);
        let id = trimmed(&id.0);
        db::interact(&self.pool, move |conn| {
            let audit = audit.for_target_tenant(&org)?;
            let event = match outcome {
                None => AuditEvent {
                    operation: "webhook.test.requested".to_owned(),
                    target_type: "webhook".to_owned(),
                    target_id: id,
                    result: "success".to_owned(),
                    classification: "external_delivery_requested".to_owned(),
                    revision: None,
                },
                Some(ok) => AuditEvent {
                    operation: "webhook.test.completed".to_owned(),
                    target_type: "webhook".to_owned(),
                    target_id: id,
                    result: if ok { "success" } else { "failure" }.to_owned(),
                    classification: if ok {
                        "external_delivery_succeeded"
                    } else {
                        "external_delivery_failed"
                    }
                    .to_owned(),
                    revision: None,
                },
            };
            mutate_in_transaction(conn, &audit_key, &audit, |_tx| Ok(((), event)))
        })
        .await
    }

    /// `remove` remains the non-audited persistence primitive used by legacy/unit callers.
    /// Production settings mutations use [`Self::remove_audited`].
    pub async fn remove(&self, org: &OrgId, id: &WebhookId) -> Result<bool, AppError> {
        let org = trimmed(&org.0);
        let id = trimmed(&id.0);
        db::interact(&self.pool, move |conn| {
            if discussion_anchor_exists(conn, &org, &id)? {
                return Err(AppError::Conflict(
                    "Remove the Discord notification-thread connection before deleting this webhook."
                        .to_owned(),
                ));
            }
            let changed = conn
                .execute(
                    "DELETE FROM org_webhooks WHERE org = ?1 AND id = ?2",
                    (&org, &id),
                )
                .map_err(internal)?;
            Ok(changed > 0)
        })
        .await
    }
}

#[allow(clippy::too_many_arguments)]
fn create_in(
    conn: &Connection,
    org: &str,
    id: &WebhookId,
    _url: &str,
    url_is_allowed: bool,
    label: &str,
    events: &str,
    stored: &StoredWebhookUrl,
) -> Result<WebhookSummary, AppError> {
    // Node's order is unknown-organization first, then invalid-URL. [lib/webhooks.js:86-87]
    if !org_exists(conn, org)? {
        return Err(AppError::Validation(format!(
            "Unknown organization \"{org}\"."
        )));
    }
    if !url_is_allowed {
        return Err(AppError::Validation(INVALID_URL_MESSAGE.to_owned()));
    }
    conn.execute(
        "INSERT INTO org_webhooks \
                 (id, org, url, url_cipher, url_nonce, url_tag, label, events) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        (
            &id.0,
            &org,
            &stored.url,
            stored.encrypted.as_ref().map(|record| &record.ciphertext),
            stored.encrypted.as_ref().map(|record| &record.nonce),
            stored.encrypted.as_ref().map(|record| &record.tag),
            &label,
            &events,
        ),
    )
    .map_err(internal)?;
    // Node re-reads the row so database defaults (`created_at`) are authoritative.
    // [lib/webhooks.js:101]
    row_by_id(conn, &id.0)?
        .map(|row| row.public())
        .ok_or(AppError::Internal)
}

impl WebhookStore {
    /// Port of `setEvents(org, id, events)` — [lib/webhooks.js:108-114].
    ///
    /// `None` means no row matched; the parsed set defaults to **empty**, not to every event.
    ///
    /// # Errors
    /// Returns a database fault.
    pub async fn set_events(
        &self,
        org: &OrgId,
        id: &WebhookId,
        events: &[WebhookEvent],
    ) -> Result<Option<WebhookSummary>, AppError> {
        let normalized = encode_events(&normalize_events(Some(events), false));
        let org = trimmed(&org.0);
        let id = trimmed(&id.0);
        db::interact(&self.pool, move |conn| {
            if !normalized.split(',').any(|event| event == "published")
                && discussion_anchor_exists(conn, &org, &id)?
            {
                return Err(AppError::Conflict(
                    "Published notifications are required by the Discord thread connection."
                        .to_owned(),
                ));
            }
            let changed = conn
                .execute(
                    "UPDATE org_webhooks SET events = ?1 WHERE org = ?2 AND id = ?3",
                    (&normalized, &org, &id),
                )
                .map_err(internal)?;
            if changed == 0 {
                return Ok(None);
            }
            Ok(row_by_id(conn, &id)?.map(|row| row.public()))
        })
        .await
    }

    /// Port of `get(id)` — internal, returns a **decrypted** URL. [lib/webhooks.js:117-119]
    ///
    /// # Errors
    /// Propagates a database fault, or [`AppError::Internal`] when an encrypted row cannot be
    /// revealed with the configured key.
    pub async fn delivery(&self, id: &WebhookId) -> Result<Option<WebhookDelivery>, AppError> {
        let id = trimmed(&id.0);
        let row = db::interact(&self.pool, move |conn| row_by_id(conn, &id)).await?;
        row.map(|row| row.delivery(&self.protection)).transpose()
    }

    /// Resolve one worker target while binding it to the durable record's tenant.
    ///
    /// Unlike [`Self::delivery`], this is intentionally typed and redacted for the background
    /// delivery path: transient database faults retry, while missing/cross-tenant references and
    /// ciphertext/key failures terminally fail without letting the worker call a provider.
    /// The long-standing [`Self::delivery`] API remains for the awaited admin webhook test path.
    pub async fn resolve_delivery(
        &self,
        id: &WebhookId,
        expected_org: &OrgId,
    ) -> Result<WebhookDelivery, WebhookDeliveryResolutionFailure> {
        let id = trimmed(&id.0);
        let expected_org = trimmed(&expected_org.0);
        if id.is_empty() || expected_org.is_empty() {
            return Err(WebhookDeliveryResolutionFailure::InvalidReference);
        }
        let row = db::interact(&self.pool, move |conn| row_by_id(conn, &id))
            .await
            .map_err(|_| WebhookDeliveryResolutionFailure::Retryable)?;
        let Some(row) = row else {
            return Err(WebhookDeliveryResolutionFailure::InvalidReference);
        };
        if row.org.0 != expected_org {
            return Err(WebhookDeliveryResolutionFailure::InvalidReference);
        }
        row.delivery(&self.protection)
            .map_err(|_| WebhookDeliveryResolutionFailure::DecryptFailed)
    }

    /// The masked view of one webhook, for callers that must not see the URL at all.
    ///
    /// # Errors
    /// Returns a database fault.
    pub async fn summary(&self, id: &WebhookId) -> Result<Option<WebhookSummary>, AppError> {
        let id = trimmed(&id.0);
        let row = db::interact(&self.pool, move |conn| row_by_id(conn, &id)).await?;
        Ok(row.map(|row| row.public()))
    }

    /// Port of `recordResult(id, ok, error)` — [lib/webhooks.js:126-129].
    ///
    /// Success clears `last_error` and stamps `last_ok_at`; failure records the message truncated
    /// to 500 UTF-16 units and leaves `last_ok_at` untouched.
    ///
    /// # Errors
    /// Returns a database fault. Callers on the delivery path deliberately ignore it: a webhook
    /// bookkeeping failure must not fail the triggering publish.
    pub async fn record_result(
        &self,
        id: &WebhookId,
        outcome: Result<(), String>,
    ) -> Result<(), AppError> {
        let id = trimmed(&id.0);
        db::interact(&self.pool, move |conn| {
            match outcome {
                Ok(()) => conn.execute(
                    "UPDATE org_webhooks SET last_ok_at = datetime('now'), last_error = NULL \
                     WHERE id = ?1",
                    (&id,),
                ),
                Err(message) => {
                    let message = if message.is_empty() {
                        DEFAULT_DELIVERY_ERROR.to_owned()
                    } else {
                        message
                    };
                    conn.execute(
                        "UPDATE org_webhooks SET last_error = ?1 WHERE id = ?2",
                        (truncate_utf16(&message, MAX_ERROR_UTF16), &id),
                    )
                }
            }
            .map_err(internal)?;
            Ok(())
        })
        .await
    }
}

/// Redacted by construction: the store owns the cipher and the pool, neither of which is
/// printable.
impl std::fmt::Debug for WebhookStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WebhookStore")
            .field("protection", self.protection.as_ref())
            .finish_non_exhaustive()
    }
}

// ---------------------------------------------------------------------------
// SQL helpers
// ---------------------------------------------------------------------------

fn rows_for_org(conn: &Connection, org: &str) -> Result<Vec<WebhookRow>, AppError> {
    let mut stmt = conn
        .prepare(&format!(
            "SELECT {ROW_COLUMNS} FROM org_webhooks WHERE org = ?1 {ORDER_BY}"
        ))
        .map_err(internal)?;
    let mapped = stmt
        .query_map((org,), WebhookRow::from_row)
        .map_err(internal)?;
    mapped
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(internal)
}

fn row_by_id(conn: &Connection, id: &str) -> Result<Option<WebhookRow>, AppError> {
    conn.query_row(
        &format!("SELECT {ROW_COLUMNS} FROM org_webhooks WHERE id = ?1"),
        (id,),
        WebhookRow::from_row,
    )
    .optional()
    .map_err(internal)
}

/// Port of `orgExists(name)` — [lib/orgs.js:53-55].
///
/// Queried directly rather than through U09's `persistence::orgs`: `create()` needs the check
/// inside its own connection, and the two modules are owned by concurrent units.
fn org_exists(conn: &Connection, org: &str) -> Result<bool, AppError> {
    conn.query_row("SELECT 1 FROM orgs WHERE name = ?1", (org,), |_| Ok(()))
        .optional()
        .map(|found: Option<()>| found.is_some())
        .map_err(internal)
}

fn discussion_anchor_exists(
    conn: &Connection,
    org: &str,
    webhook_id: &str,
) -> Result<bool, AppError> {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM org_discord_discussion_connections \
         WHERE org = ?1 AND notification_webhook_id = ?2 \
           AND strategy = 'notification_thread')",
        (org, webhook_id),
        |row| row.get(0),
    )
    .map_err(internal)
}

/// `String(value || "").trim()` — the normalization every Node entry point applies.
fn trimmed(value: &str) -> String {
    value.trim().to_owned()
}

/// Database faults never leak SQL, row contents, or a URL.
fn internal(error: rusqlite::Error) -> AppError {
    tracing::error!(error = %error, "webhook persistence query failed");
    AppError::Internal
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persistence::migrations::mask_webhook_url;

    #[test]
    fn event_names_match_the_frozen_order() {
        assert_eq!(
            EVENTS.iter().map(event_name).collect::<Vec<&str>>(),
            [
                "published",
                "updated",
                "restored",
                "deleted",
                "feedback",
                "resolved"
            ]
        );
        for event in &EVENTS {
            assert_eq!(event_from_name(event_name(event)).as_ref(), Some(event));
        }
        assert_eq!(event_from_name("nope"), None);
    }

    #[test]
    fn stored_events_drop_unknown_names() {
        assert_eq!(
            parse_stored_events("published,nope,resolved"),
            [WebhookEvent::Published, WebhookEvent::Resolved]
        );
        assert_eq!(parse_stored_events(""), Vec::<WebhookEvent>::new());
        assert_eq!(
            encode_events(&EVENTS),
            "published,updated,restored,deleted,feedback,resolved"
        );
    }

    #[test]
    fn normalization_defaults_and_deduplicates_in_order() {
        assert_eq!(normalize_events(None, true), EVENTS.to_vec());
        assert_eq!(normalize_events(None, false), Vec::<WebhookEvent>::new());
        assert_eq!(
            normalize_events(
                Some(&[
                    WebhookEvent::Resolved,
                    WebhookEvent::Published,
                    WebhookEvent::Resolved
                ]),
                true
            ),
            [WebhookEvent::Resolved, WebhookEvent::Published]
        );
    }

    #[test]
    fn display_mask_omits_the_slash_the_stored_mask_keeps() {
        let url = "https://discord.com/api/webhooks/123456789012345678/secret-token";
        assert_eq!(mask_url(url), "https://discord.com…oken");
        assert_eq!(mask_webhook_url(url), "https://discord.com/…oken");
        // Masking the already-masked stored value is what `publicRow` actually does.
        assert_eq!(mask_url(&mask_webhook_url(url)), "https://discord.com…oken");
        assert_eq!(
            mask_url("https://hooks.example.com:8443/abc/secret"),
            "https://hooks.example.com:8443…cret"
        );
        assert_eq!(mask_url("not a url"), "… url");
        assert_eq!(mask_url("abc"), "…abc");
        assert_eq!(mask_url(""), "…");
    }

    #[test]
    fn allowlist_accepts_only_the_two_discord_prefixes() {
        for accepted in [
            "https://discord.com/api/webhooks/1/t",
            "https://discordapp.com/api/webhooks/1/t",
            "HTTPS://DISCORD.COM/API/WEBHOOKS/1/t",
            "https://Discord.com/api/webhooks/",
        ] {
            assert!(is_discord_webhook_url(accepted), "rejected {accepted}");
        }
        for rejected in [
            "",
            "http://discord.com/api/webhooks/1/t",
            "https://discord.com.evil.tld/api/webhooks/1/t",
            "https://evil.tld/https://discord.com/api/webhooks/1/t",
            "https://discord.com@evil.tld/api/webhooks/1/t",
            "https://discord.com/api/webhook/1/t",
            "https://discordapp.com.evil/api/webhooks/1/t",
            "https://169.254.169.254/api/webhooks/1/t",
            " https://discord.com/api/webhooks/1/t",
        ] {
            assert!(!is_discord_webhook_url(rejected), "accepted {rejected}");
        }
    }

    #[test]
    fn utf16_slicing_matches_javascript_for_ascii_and_keeps_pairs_whole() {
        assert_eq!(last_four_utf16("abcdef"), "cdef");
        assert_eq!(last_four_utf16("abc"), "abc");
        assert_eq!(
            truncate_utf16(&"x".repeat(200), MAX_LABEL_UTF16),
            "x".repeat(80)
        );
        assert_eq!(truncate_utf16("short", MAX_LABEL_UTF16), "short");
        // "🎉" is two UTF-16 units: a one-unit budget cannot include it.
        assert_eq!(truncate_utf16("🎉", 1), "");
        assert_eq!(truncate_utf16("a🎉", 2), "a");
        assert_eq!(truncate_utf16("a🎉", 3), "a🎉");
    }
}
