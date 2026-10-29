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

mod events;
mod rows;
mod sql_helpers;
mod store;
mod url_rules;

pub use events::*;
pub use rows::*;
pub use store::*;
pub use url_rules::*;
