//! Encrypted webhook persistence.
//!
//! Webhook persistence and the notification adapter are the only two
//! places a full Discord webhook URL is ever materialised; everything else sees the masked form.
//!
//! # At-rest layout, migration v18
//!
//! | Column | With a key | Without a key |
//! |---|---|---|
//! | `url` | masked display value (`storedMaskUrl`) | the URL **verbatim** |
//! | `url_cipher`/`url_nonce`/`url_tag` | AES-256-GCM record | `NULL` |
//!
//! `WEBHOOK_ENC_KEY` is currently unset in production, so the plaintext column is a live configuration and not a legacy path. Both are modelled by [`WebhookUrlProtection`], which this module consumes rather than reimplementing.
//!
//! # Two different masks
//!
//! The stored and public URL masks differ by one character:
//!
//! * `storedMaskUrl` — `{scheme}://{host}/…{last4}` — what is written to the `url` column when a row is encrypted. Already ported as [`mask_webhook_url`](crate::persistence::migrations::mask_webhook_url) and applied by [`WebhookUrlProtection::protect`].
//! * `maskUrl` — `{scheme}://{host}…{last4}` (no slash) — what [`WebhookRow::public`] shows. It is applied to whatever is in the `url` column, so an encrypted row is masked *twice*: the stored `https://discord.com/…oken` renders as `https://discord.com…oken`. The admin UI depends on this frozen display format, originally produced by the retired Node twin.
//!
//! # Secret containment
//!
//! * [`WebhookRow`] and [`WebhookStore`] have hand-written `Debug` impls that redact the URL, so no accidental `{:?}` can print a token.
//! * No error returned from this module embeds a URL: the reveal failure path collapses to [`AppError::Internal`] inside the webhook cipher.
//! * [`WebhookSummary`] — the only shape that reaches an HTTP response — always carries the masked value.

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

/// Every webhook event, in the frozen wire order.
pub const EVENTS: [WebhookEvent; 6] = [
    WebhookEvent::Published,
    WebhookEvent::Updated,
    WebhookEvent::Restored,
    WebhookEvent::Deleted,
    WebhookEvent::Feedback,
    WebhookEvent::Resolved,
];

/// Frozen rejection message for a URL outside the Discord allowlist.
pub const INVALID_URL_MESSAGE: &str = "Webhook URL must be an HTTPS Discord webhook URL.";

/// Maximum webhook label length after trimming, in UTF-16 code units.
pub const MAX_LABEL_UTF16: usize = 80;

/// `String(error).slice(0, 500)`.
pub const MAX_ERROR_UTF16: usize = 500;

/// Default recorded failure text.
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

/// Frozen ordering from `listForOrgStmt`.
const ORDER_BY: &str = "ORDER BY created_at ASC, id ASC";

mod events;
mod rows;
mod sql_helpers;
mod store;
mod url_rules;

pub use events::*;
pub use rows::*;
pub use store::*;
pub use url_rules::*;
