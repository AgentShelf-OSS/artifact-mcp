//! Owned by U12 (terra) — Discord payloads and guarded webhook delivery.
//!
//! Port of `lib/notify.js`. Three properties here are load-bearing and each is pinned by a test:
//!
//! 1. **Byte-identical embeds.** Discord renders what it is sent, so the JSON must match
//!    `JSON.stringify(buildEmbed(event, payload))` exactly — including key order, which is why the
//!    payload is a `serde` struct tree (declaration order) and not a `serde_json::Map`
//!    (alphabetical). `tests/native/u12_node_parity.rs` compares against the real `lib/notify.js`.
//! 2. **The SSRF guard.** Delivery targets are restricted to `https://discord.com/api/webhooks/`
//!    and `https://discordapp.com/api/webhooks/` by
//!    [`is_discord_webhook_url`](crate::persistence::webhooks::is_discord_webhook_url), redirects
//!    are refused rather than followed, and every request is bounded by [`DELIVERY_TIMEOUT`].
//!    A prior audit cleared the Node predicate; it is ported literally, not "improved".
//! 3. **Detached delivery.** [`DiscordNotifier::emit`] resolves before any request completes and
//!    can never return an error, so a dead Discord endpoint cannot fail a publish or an update.
//!
//! # Where the guard lives
//!
//! Node enforces the allowlist only in `webhooks.create()`. This port *also* re-checks at delivery
//! time. That is strictly narrower than Node — the delivery check can only reject a URL that
//! `create` would already have rejected — and it means a row hand-edited into the database, or one
//! whose plaintext `url` column was tampered with, still cannot be used to reach an internal host.
//!
//! # Secret containment
//!
//! The webhook URL is passed to the transport and to nothing else. No type in this module holds it
//! in a `Debug`-visible field, no error text embeds it, and no `tracing` event carries it.

use std::{fmt, sync::Arc, time::Duration};

use serde::Serialize;

use crate::{
    config::{OsRandom, RandomSource},
    error::AppError,
    model::{DeliveryResult, NotificationPayload, OrgId, WebhookDelivery, WebhookEvent},
    persistence::webhooks::{
        INVALID_URL_MESSAGE, WebhookStore, is_discord_webhook_url, truncate_utf16, utf16_len,
    },
    ports::{BoxFuture, NotificationSink},
};

/// `setTimeout(() => controller.abort(), 4000)` — [lib/notify.js:61].
pub const DELIVERY_TIMEOUT: Duration = Duration::from_millis(4000);

/// Verbatim Node fallback when a delivery fails without a message. [lib/notify.js:88]
pub const GENERIC_FAILURE: &str = "Webhook delivery failed.";

/// Verbatim Node result for `test()` on a webhook with no URL. [lib/notify.js:105]
pub const UNKNOWN_WEBHOOK: &str = "Unknown webhook.";

/// Recorded when the endpoint answers with a 3xx. Node's `redirect: "error"` makes `fetch` reject
/// with an opaque `"fetch failed"`; this message says what actually happened. Only `last_error`
/// text differs — the security decision (never follow) is identical.
pub const REDIRECT_REFUSED: &str = "Discord returned a redirect, which is not followed.";

/// Recorded when a stored URL is not on the Discord allowlist. Reuses the `create()` wording so an
/// operator sees the same sentence in both places.
pub const URL_NOT_ALLOWED: &str = INVALID_URL_MESSAGE;

/// Attachment file name for the optional preview image. [lib/notify.js:71]
pub const PREVIEW_FILENAME: &str = "preview.png";

/// `text(value, max)` limits — [lib/notify.js:30,39-54].
const TITLE_MAX: usize = 256;
const AUTHOR_MAX: usize = 256;
const DESCRIPTION_MAX: usize = 2048;
const FIELD_MAX: usize = 1024;

// ---------------------------------------------------------------------------
// Embed construction
// ---------------------------------------------------------------------------

/// Port of `COLORS` — [lib/notify.js:6-13].
#[must_use]
pub const fn event_color(event: &WebhookEvent) -> u32 {
    match *event {
        WebhookEvent::Published => 0x002f_9e74,
        WebhookEvent::Updated => 0x003b_82f6,
        WebhookEvent::Restored => 0x008b_5cf6,
        WebhookEvent::Deleted => 0x00dc_2626,
        WebhookEvent::Feedback => 0x00f5_9e0b,
        WebhookEvent::Resolved => 0x0016_a34a,
    }
}

/// The top-level POST body: `{ embeds: [embed] }` — [lib/notify.js:56].
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DiscordPayload {
    /// Exactly one embed, as the reference always builds.
    pub embeds: Vec<Embed>,
}

/// One Discord embed.
///
/// **Field order is the wire format.** `lib/notify.js` builds the object literal as
/// `{color, author, title, url?, fields}` and then assigns `description` (and, for the preview
/// path, `image`) afterwards, so both land *after* `fields` in `JSON.stringify` output. Reordering
/// these declarations changes the bytes Discord receives.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Embed {
    /// `COLORS[event]`.
    pub color: u32,
    /// `{ name }` only.
    pub author: EmbedAuthor,
    /// Truncated title.
    pub title: String,
    /// Present only when the payload carries a non-empty URL. [lib/notify.js:35]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Event-specific fields; may be empty.
    pub fields: Vec<EmbedField>,
    /// Assigned after `fields` in every branch of the reference.
    pub description: String,
    /// Assigned last, only on the preview path. [lib/notify.js:68]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<EmbedImage>,
}

/// `author: { name }` — [lib/notify.js:33].
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct EmbedAuthor {
    /// Organization label, or `Artifact Index`.
    pub name: String,
}

/// One `{ name, value, inline }` field, in the reference's literal order.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct EmbedField {
    /// Field label.
    pub name: String,
    /// Field body; always a JSON string, even for numeric values.
    pub value: String,
    /// Always `true` in the reference.
    pub inline: bool,
}

/// `image: { url: "attachment://preview.png" }` — [lib/notify.js:68].
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct EmbedImage {
    /// Always the `attachment://` form.
    pub url: String,
}

/// Port of `text(value, max)` — [lib/notify.js:15-18].
///
/// Trim first, then truncate to `max` UTF-16 units by keeping `max - 1` and appending `…`.
#[must_use]
pub fn text(value: &str, max: usize) -> String {
    let trimmed = value.trim();
    if utf16_len(trimmed) <= max {
        return trimmed.to_owned();
    }
    format!("{}…", truncate_utf16(trimmed, max.saturating_sub(1)))
}

/// Port of `bytes(value)` — [lib/notify.js:20-26].
///
/// `toFixed(1)` is **not** `format!("{:.1}")`: ECMAScript rounds a tie to the larger integer
/// (away from zero) while Rust rounds to even, so `1280` renders as `1.3 KB` in Node and would
/// render as `1.2 KB` from a naive port. The quotient is an exact rational here (the divisor is a
/// power of two), so the rounding is done in integer arithmetic and is exact for both runtimes.
#[must_use]
pub fn format_bytes(value: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * 1024;
    if value < KIB {
        format!("{value} B")
    } else if value < MIB {
        format!("{} KB", to_fixed_one(value, KIB))
    } else {
        format!("{} MB", to_fixed_one(value, MIB))
    }
}

/// `(numerator / denominator).toFixed(1)` with ECMAScript's round-half-up tie rule.
fn to_fixed_one(numerator: u64, denominator: u64) -> String {
    let scaled = u128::from(numerator) * 10;
    let divisor = u128::from(denominator);
    let mut tenths = scaled / divisor;
    if (scaled % divisor) * 2 >= divisor {
        tenths += 1;
    }
    format!("{}.{}", tenths / 10, tenths % 10)
}

/// Port of `buildEmbed(event, payload)` — [lib/notify.js:28-57].
///
/// `org` is a separate argument because the reference injects it into the payload at the call site
/// (`emit` does `{ ...payload, org }`, `test` passes `webhookRow.org`), and the frozen
/// [`NotificationPayload`] has no `org` field.
#[must_use]
pub fn build_embed(
    event: &WebhookEvent,
    org: &OrgId,
    payload: &NotificationPayload,
) -> DiscordPayload {
    let title_source = if payload.title.is_empty() {
        default_title(event)
    } else {
        payload.title.as_str()
    };
    let author = if org.0.is_empty() {
        "Artifact Index"
    } else {
        org.0.as_str()
    };

    let (description, fields) = match *event {
        WebhookEvent::Published
        | WebhookEvent::Updated
        | WebhookEvent::Restored
        | WebhookEvent::Deleted => {
            let description = or_else(text(&payload.description, DESCRIPTION_MAX), || {
                format!("{} artifact", capitalize(event_word(event)))
            });
            let fields = vec![
                field("Publisher", &or_default(&payload.uploader_label, "Unknown")),
                field("Category", &or_default(&payload.category, "Uncategorized")),
                field("Revision", &revision(payload.revision)),
                field("Size", &format_bytes(payload.bytes)),
            ];
            (description, fields)
        }
        WebhookEvent::Feedback => {
            let body = payload.body.as_deref().unwrap_or_default();
            let description = or_else(text(body, DESCRIPTION_MAX), || "(No message)".to_owned());
            let viewer = payload
                .viewer_email
                .as_ref()
                .map_or("", |email| email.0.as_str());
            let fields = vec![
                field("Viewer", &or_default(viewer, "Unknown")),
                field("Revision", &revision(payload.revision)),
            ];
            (description, fields)
        }
        WebhookEvent::Resolved => {
            let resolver = payload.resolver.as_deref().unwrap_or_default();
            let fields = vec![field("Resolver", &or_default(resolver, "Unknown"))];
            ("Feedback resolved".to_owned(), fields)
        }
    };

    DiscordPayload {
        embeds: vec![Embed {
            color: event_color(event),
            author: EmbedAuthor {
                name: text(author, AUTHOR_MAX),
            },
            title: text(title_source, TITLE_MAX),
            // `payload.url ? { url: String(payload.url) } : {}` — an empty URL omits the key, and
            // the value is *not* trimmed or truncated.
            url: (!payload.url.is_empty()).then(|| payload.url.clone()),
            fields,
            description,
            image: None,
        }],
    }
}

/// `["published","updated","restored"].includes(event)` — [lib/notify.js:64].
#[must_use]
pub const fn accepts_preview(event: &WebhookEvent) -> bool {
    matches!(
        *event,
        WebhookEvent::Published | WebhookEvent::Updated | WebhookEvent::Restored
    )
}

/// The `event === "feedback" ? … : event === "resolved" ? … : "Artifact"` ladder.
/// [lib/notify.js:30]
const fn default_title(event: &WebhookEvent) -> &'static str {
    match *event {
        WebhookEvent::Feedback => "New feedback",
        WebhookEvent::Resolved => "Feedback resolved",
        _ => "Artifact",
    }
}

/// The lowercase event word used to build the default artifact description.
const fn event_word(event: &WebhookEvent) -> &'static str {
    match *event {
        WebhookEvent::Published => "published",
        WebhookEvent::Updated => "updated",
        WebhookEvent::Restored => "restored",
        WebhookEvent::Deleted => "deleted",
        WebhookEvent::Feedback => "feedback",
        WebhookEvent::Resolved => "resolved",
    }
}

/// `${word[0].toUpperCase()}${word.slice(1)}` for the ASCII event words.
fn capitalize(word: &str) -> String {
    let mut characters = word.chars();
    characters.next().map_or_else(String::new, |first| {
        first.to_uppercase().collect::<String>() + characters.as_str()
    })
}

/// `text(value || fallback, 1024)` — the `||` is applied *before* trimming, so a whitespace-only
/// value collapses to the empty string rather than to the fallback. That is the reference
/// behaviour. [lib/notify.js:41-54]
fn or_default(value: &str, fallback: &str) -> String {
    text(if value.is_empty() { fallback } else { value }, FIELD_MAX)
}

/// `text(...) || fallback` — the `||` is applied *after* trimming/truncation.
fn or_else(value: String, fallback: impl FnOnce() -> String) -> String {
    if value.is_empty() { fallback() } else { value }
}

/// `String(payload.revision || 1)` — [lib/notify.js:43].
fn revision(value: u64) -> String {
    if value == 0 {
        "1".to_owned()
    } else {
        value.to_string()
    }
}

fn field(name: &str, value: &str) -> EmbedField {
    EmbedField {
        name: name.to_owned(),
        value: value.to_owned(),
        inline: true,
    }
}

// ---------------------------------------------------------------------------
// Transport
// ---------------------------------------------------------------------------

/// A single outbound POST body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeliveryRequest {
    /// `content-type` header value.
    pub content_type: String,
    /// Serialized body bytes.
    pub body: Vec<u8>,
}

/// What the transport observed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeliveryResponse {
    /// HTTP status code.
    pub status: u16,
}

impl DeliveryResponse {
    /// `response.ok` — a 2xx status. [lib/notify.js:84]
    #[must_use]
    pub const fn ok(self) -> bool {
        self.status >= 200 && self.status < 300
    }

    /// A 3xx status, which this client refuses rather than follows.
    #[must_use]
    pub const fn is_redirect(self) -> bool {
        self.status >= 300 && self.status < 400
    }
}

/// The `fetchImpl` seam of `lib/notify.js`, so delivery policy can be tested without a network.
///
/// Implementations **must not** log or otherwise reproduce `url`: it is the webhook secret.
pub trait WebhookTransport: Send + Sync + fmt::Debug {
    /// POST `request` to `url`, returning the status or a failure message.
    ///
    /// The error is a message string rather than an [`AppError`] because it is recorded verbatim
    /// in `org_webhooks.last_error`, mirroring `String(error?.message || …)`.
    fn post<'a>(
        &'a self,
        url: &'a str,
        request: DeliveryRequest,
    ) -> BoxFuture<'a, Result<DeliveryResponse, String>>;
}

/// Production transport: Rustls `reqwest`, redirects refused, request bounded by a timeout.
pub struct HttpTransport {
    client: reqwest::Client,
    timeout: Duration,
}

impl HttpTransport {
    /// Build the production client with [`DELIVERY_TIMEOUT`].
    ///
    /// # Errors
    /// Returns [`AppError::Unavailable`] when the TLS backend cannot be initialised.
    pub fn new() -> Result<Self, AppError> {
        Self::with_timeout(DELIVERY_TIMEOUT)
    }

    /// [`HttpTransport::new`] with an explicit timeout, so tests need not wait four seconds.
    ///
    /// # Errors
    /// As [`HttpTransport::new`].
    pub fn with_timeout(timeout: Duration) -> Result<Self, AppError> {
        let client = reqwest::Client::builder()
            // `redirect: "error"` — the request must never be replayed against a host the
            // allowlist did not approve.
            .redirect(reqwest::redirect::Policy::none())
            .timeout(timeout)
            .build()
            .map_err(|error| {
                tracing::error!(error = %error, "webhook http client could not be built");
                AppError::Unavailable("webhook delivery client unavailable".to_owned())
            })?;
        Ok(Self { client, timeout })
    }
}

impl fmt::Debug for HttpTransport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HttpTransport")
            .field("timeout", &self.timeout)
            .finish_non_exhaustive()
    }
}

impl WebhookTransport for HttpTransport {
    fn post<'a>(
        &'a self,
        url: &'a str,
        request: DeliveryRequest,
    ) -> BoxFuture<'a, Result<DeliveryResponse, String>> {
        Box::pin(async move {
            let send = self
                .client
                .post(url)
                .header("content-type", request.content_type)
                .body(request.body)
                .send();
            // Belt and braces: `reqwest`'s own timeout covers the request, and this bounds the
            // whole operation regardless of how the client handles connect/DNS stalls.
            let response = tokio::time::timeout(self.timeout, send)
                .await
                .map_err(|_| "Webhook delivery timed out.".to_owned())?
                // `error.message` in Node is opaque for network faults; reqwest's Display can
                // contain the URL, so it is deliberately not propagated.
                .map_err(|error| {
                    tracing::warn!(
                        timeout_ms = u64::try_from(self.timeout.as_millis()).unwrap_or(u64::MAX),
                        kind = describe(&error),
                        "webhook delivery request failed"
                    );
                    format!("Webhook delivery failed ({}).", describe(&error))
                })?;
            let observed = DeliveryResponse {
                status: response.status().as_u16(),
            };
            if observed.is_redirect() {
                return Err(REDIRECT_REFUSED.to_owned());
            }
            Ok(observed)
        })
    }
}

/// A URL-free classification of a `reqwest` failure, safe to log and to store in `last_error`.
fn describe(error: &reqwest::Error) -> &'static str {
    if error.is_timeout() {
        "timed out"
    } else if error.is_connect() {
        "connection refused"
    } else if error.is_redirect() {
        "redirected"
    } else if error.is_body() || error.is_decode() {
        "bad response body"
    } else {
        "request error"
    }
}

// ---------------------------------------------------------------------------
// Notifier
// ---------------------------------------------------------------------------

/// [`NotificationSink`] implementation: builds Discord payloads and delivers them detached.
#[derive(Clone)]
pub struct DiscordNotifier {
    webhooks: Arc<WebhookStore>,
    transport: Arc<dyn WebhookTransport>,
    random: Arc<dyn RandomSource>,
}

impl DiscordNotifier {
    /// Build a notifier over a store and a transport.
    #[must_use]
    pub fn new(webhooks: Arc<WebhookStore>, transport: Arc<dyn WebhookTransport>) -> Self {
        Self::with_random(webhooks, transport, Arc::new(OsRandom))
    }

    /// [`DiscordNotifier::new`] with an injected source for the multipart boundary.
    #[must_use]
    pub const fn with_random(
        webhooks: Arc<WebhookStore>,
        transport: Arc<dyn WebhookTransport>,
        random: Arc<dyn RandomSource>,
    ) -> Self {
        Self {
            webhooks,
            transport,
            random,
        }
    }

    /// Port of `emit(event, org, payload, { preview })` — [lib/notify.js:96-101].
    ///
    /// Returns as soon as the subscriber rows are known; every delivery runs on its own detached
    /// task. Nothing about a webhook — an unreachable host, a decryption failure, a database
    /// error — can propagate to the caller, which is why the return type is always `Ok(())`.
    pub async fn emit_with_preview(
        &self,
        event: WebhookEvent,
        org: OrgId,
        payload: NotificationPayload,
        preview: Option<Arc<Vec<u8>>>,
    ) {
        // Node wraps the whole body in `try {} catch {}`: a failure to even list the webhooks is
        // swallowed. Reproduced here, with a log so the failure is still observable.
        let deliveries = match self.webhooks.for_event(&org, &event).await {
            Ok(deliveries) => deliveries,
            Err(error) => {
                tracing::warn!(
                    org = %org,
                    event = ?event,
                    error = %error,
                    "webhook subscribers could not be resolved; notification dropped"
                );
                return;
            }
        };
        for delivery in deliveries {
            let notifier = self.clone();
            let event = event.clone();
            let org = org.clone();
            let payload = payload.clone();
            let preview = preview.clone();
            // `void deliver(...)` — detached, never awaited by the caller.
            tokio::spawn(async move {
                notifier
                    .deliver(
                        &delivery,
                        &event,
                        &org,
                        &payload,
                        preview.as_deref().map(Vec::as_slice),
                    )
                    .await;
            });
        }
    }

    /// Port of `deliver(row, event, payload, fetchImpl, preview)` — [lib/notify.js:59-94].
    ///
    /// Awaitable, unlike [`DiscordNotifier::emit_with_preview`]: `test()` and the unit tests need
    /// the outcome. The result is recorded on the row before it is returned.
    pub async fn deliver(
        &self,
        webhook: &WebhookDelivery,
        event: &WebhookEvent,
        org: &OrgId,
        payload: &NotificationPayload,
        preview: Option<&[u8]>,
    ) -> DeliveryResult {
        let outcome = self.attempt(webhook, event, org, payload, preview).await;
        let recorded = match &outcome {
            Ok(()) => Ok(()),
            Err(message) => Err(message.clone()),
        };
        // `try { webhooks.recordResult(...) } catch {}` — bookkeeping never changes the outcome.
        if let Err(error) = self.webhooks.record_result(&webhook.id, recorded).await {
            tracing::warn!(
                webhook = %webhook.id,
                error = %error,
                "webhook delivery result could not be recorded"
            );
        }
        match outcome {
            Ok(()) => DeliveryResult {
                ok: true,
                error: None,
            },
            Err(message) => DeliveryResult {
                ok: false,
                error: Some(message),
            },
        }
    }

    async fn attempt(
        &self,
        webhook: &WebhookDelivery,
        event: &WebhookEvent,
        org: &OrgId,
        payload: &NotificationPayload,
        preview: Option<&[u8]>,
    ) -> Result<(), String> {
        // The SSRF guard, re-applied at the moment of use. See the module docs.
        if !is_discord_webhook_url(&webhook.url) {
            tracing::error!(
                webhook = %webhook.id,
                "refusing to deliver a webhook whose stored URL is not a Discord endpoint"
            );
            return Err(URL_NOT_ALLOWED.to_owned());
        }

        let mut body = build_embed(event, org, payload);
        let request = match preview.filter(|bytes| !bytes.is_empty() && accepts_preview(event)) {
            Some(image) => {
                let embed = body.embeds.first_mut().ok_or(GENERIC_FAILURE.to_owned())?;
                embed.image = Some(EmbedImage {
                    url: format!("attachment://{PREVIEW_FILENAME}"),
                });
                let json = serde_json::to_string(&body).map_err(|_| GENERIC_FAILURE.to_owned())?;
                multipart_request(&json, image, &self.boundary())
            }
            None => DeliveryRequest {
                content_type: "application/json".to_owned(),
                body: serde_json::to_vec(&body).map_err(|_| GENERIC_FAILURE.to_owned())?,
            },
        };

        let response = self.transport.post(&webhook.url, request).await?;
        if response.ok() {
            Ok(())
        } else {
            // `throw new Error(\`Discord returned HTTP ${response.status}\`)` — [lib/notify.js:84]
            Err(format!("Discord returned HTTP {}", response.status))
        }
    }

    /// A fresh multipart boundary. Falls back to a fixed token if the entropy source fails, which
    /// is harmless: the boundary is a framing detail, not a secret.
    fn boundary(&self) -> String {
        let mut bytes = [0_u8; 16];
        if self.random.fill_bytes(&mut bytes).is_err() {
            return "artifactmcpwebhookboundary0000000".to_owned();
        }
        let mut boundary = String::with_capacity(32);
        for byte in bytes {
            boundary.push_str(&format!("{byte:02x}"));
        }
        boundary
    }
}

impl fmt::Debug for DiscordNotifier {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DiscordNotifier")
            .field("transport", &self.transport)
            .finish_non_exhaustive()
    }
}

/// Port of the `FormData` branch — [lib/notify.js:69-72].
///
/// `undici` serializes `FormData` as RFC 7578 multipart with two parts: `payload_json` (the embed
/// JSON) and `files[0]` (the PNG, filename `preview.png`, type `image/png`). The bytes are built
/// here rather than with a helper crate so the framing is explicit and testable.
#[must_use]
pub fn multipart_request(payload_json: &str, preview: &[u8], boundary: &str) -> DeliveryRequest {
    let mut body = Vec::with_capacity(payload_json.len() + preview.len() + 256);
    body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    body.extend_from_slice(b"Content-Disposition: form-data; name=\"payload_json\"\r\n\r\n");
    body.extend_from_slice(payload_json.as_bytes());
    body.extend_from_slice(format!("\r\n--{boundary}\r\n").as_bytes());
    body.extend_from_slice(
        format!(
            "Content-Disposition: form-data; name=\"files[0]\"; filename=\"{PREVIEW_FILENAME}\"\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(b"Content-Type: image/png\r\n\r\n");
    body.extend_from_slice(preview);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    DeliveryRequest {
        content_type: format!("multipart/form-data; boundary={boundary}"),
        body,
    }
}

impl NotificationSink for DiscordNotifier {
    fn emit(
        &self,
        event: WebhookEvent,
        org: OrgId,
        payload: NotificationPayload,
    ) -> BoxFuture<'_, Result<(), AppError>> {
        Box::pin(async move {
            self.emit_with_preview(event, org, payload, None).await;
            // Always `Ok`: notification failures are isolated from the triggering operation.
            Ok(())
        })
    }

    fn test<'a>(
        &'a self,
        webhook: &'a WebhookDelivery,
    ) -> BoxFuture<'a, Result<DeliveryResult, AppError>> {
        Box::pin(async move {
            // `if (!webhookRow?.url) return { ok: false, error: "Unknown webhook." }`
            if webhook.url.is_empty() {
                return Ok(DeliveryResult {
                    ok: false,
                    error: Some(UNKNOWN_WEBHOOK.to_owned()),
                });
            }
            // The admin Test button deliberately awaits this one. [lib/notify.js:103-115]
            Ok(self
                .deliver(
                    webhook,
                    &WebhookEvent::Published,
                    &webhook.org,
                    &test_payload(),
                    None,
                )
                .await)
        })
    }
}

/// The literal payload `test()` sends — [lib/notify.js:107-114].
///
/// `url` is the hard-coded `http://localhost:3480`, not the configured public base URL; that is
/// the reference behaviour and is ported verbatim.
#[must_use]
pub fn test_payload() -> NotificationPayload {
    NotificationPayload {
        artifact_id: crate::model::ArtifactId(String::new()),
        title: "Webhook test".to_owned(),
        url: "http://localhost:3480".to_owned(),
        description: String::new(),
        uploader_label: "Artifact Index".to_owned(),
        category: "Notifications".to_owned(),
        revision: 1,
        bytes: 0,
        viewer_email: None,
        body: None,
        resolver: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload() -> NotificationPayload {
        NotificationPayload {
            artifact_id: crate::model::ArtifactId("abc123".to_owned()),
            title: "Quarterly report".to_owned(),
            url: "https://example.test/abc123".to_owned(),
            description: "A description".to_owned(),
            uploader_label: "Ada".to_owned(),
            category: "Reports".to_owned(),
            revision: 3,
            bytes: 2048,
            viewer_email: Some(crate::model::EmailAddress("v@example.test".to_owned())),
            body: Some("Nice work".to_owned()),
            resolver: Some("Grace".to_owned()),
        }
    }

    #[test]
    fn embed_key_order_is_the_reference_order() {
        let json = serde_json::to_string(&build_embed(
            &WebhookEvent::Published,
            &OrgId("acme".to_owned()),
            &payload(),
        ))
        .expect("serialize");
        assert_eq!(
            json,
            concat!(
                r#"{"embeds":[{"color":3120756,"author":{"name":"acme"},"title":"Quarterly report","#,
                r#""url":"https://example.test/abc123","fields":["#,
                r#"{"name":"Publisher","value":"Ada","inline":true},"#,
                r#"{"name":"Category","value":"Reports","inline":true},"#,
                r#"{"name":"Revision","value":"3","inline":true},"#,
                r#"{"name":"Size","value":"2.0 KB","inline":true}],"#,
                r#""description":"A description"}]}"#
            )
        );
    }

    #[test]
    fn colors_match_the_reference_table() {
        assert_eq!(
            EVENTS_FOR_TEST.map(|event| event_color(&event)),
            [
                0x002f_9e74,
                0x003b_82f6,
                0x008b_5cf6,
                0x00dc_2626,
                0x00f5_9e0b,
                0x0016_a34a
            ]
        );
    }

    const EVENTS_FOR_TEST: [WebhookEvent; 6] = crate::persistence::webhooks::EVENTS;

    #[test]
    fn byte_formatting_uses_javascript_tie_rounding() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(1023), "1023 B");
        assert_eq!(format_bytes(1024), "1.0 KB");
        // (1280 / 1024) === 1.25 exactly; toFixed(1) picks the larger tenth, "{:.1}" would not.
        assert_eq!(format_bytes(1280), "1.3 KB");
        assert_eq!(format_bytes(1024 + 768), "1.8 KB");
        assert_eq!(format_bytes(1_048_575), "1024.0 KB");
        assert_eq!(format_bytes(1_048_576), "1.0 MB");
        assert_eq!(format_bytes(1_048_576 + 262_144), "1.3 MB");
    }

    #[test]
    fn text_trims_then_truncates_with_an_ellipsis() {
        assert_eq!(text("  spaced  ", 256), "spaced");
        assert_eq!(text(&"a".repeat(10), 4), "aaa…");
        assert_eq!(text(&"a".repeat(4), 4), "aaaa");
        assert_eq!(text("   ", 256), "");
    }

    #[test]
    fn empty_values_fall_back_exactly_as_the_reference_does() {
        let mut input = payload();
        input.title = String::new();
        input.description = "   ".to_owned();
        input.uploader_label = String::new();
        input.category = String::new();
        input.revision = 0;
        input.url = String::new();

        let built = build_embed(&WebhookEvent::Deleted, &OrgId(String::new()), &input);
        let embed = built.embeds.first().expect("one embed");
        assert_eq!(embed.title, "Artifact");
        assert_eq!(embed.author.name, "Artifact Index");
        assert_eq!(embed.description, "Deleted artifact");
        assert_eq!(embed.url, None);
        assert_eq!(embed.fields[0].value, "Unknown");
        assert_eq!(embed.fields[1].value, "Uncategorized");
        assert_eq!(embed.fields[2].value, "1");
    }

    #[test]
    fn feedback_and_resolved_use_their_own_shapes() {
        let mut input = payload();
        input.body = None;
        input.viewer_email = None;
        let built = build_embed(&WebhookEvent::Feedback, &OrgId("acme".to_owned()), &input);
        let embed = built.embeds.first().expect("one embed");
        assert_eq!(embed.description, "(No message)");
        assert_eq!(embed.fields.len(), 2);
        assert_eq!(embed.fields[0].name, "Viewer");
        assert_eq!(embed.fields[0].value, "Unknown");

        let mut input = payload();
        input.resolver = None;
        let built = build_embed(&WebhookEvent::Resolved, &OrgId("acme".to_owned()), &input);
        let embed = built.embeds.first().expect("one embed");
        assert_eq!(embed.description, "Feedback resolved");
        assert_eq!(embed.fields.len(), 1);
        assert_eq!(embed.fields[0].name, "Resolver");
        assert_eq!(embed.fields[0].value, "Unknown");
    }

    #[test]
    fn preview_is_only_offered_for_the_three_artifact_write_events() {
        assert!(accepts_preview(&WebhookEvent::Published));
        assert!(accepts_preview(&WebhookEvent::Updated));
        assert!(accepts_preview(&WebhookEvent::Restored));
        assert!(!accepts_preview(&WebhookEvent::Deleted));
        assert!(!accepts_preview(&WebhookEvent::Feedback));
        assert!(!accepts_preview(&WebhookEvent::Resolved));
    }

    #[test]
    fn multipart_framing_matches_the_form_data_shape() {
        let request = multipart_request("{\"a\":1}", &[0x89, 0x50], "BOUND");
        assert_eq!(request.content_type, "multipart/form-data; boundary=BOUND");
        let body = String::from_utf8_lossy(&request.body).into_owned();
        assert!(body.starts_with(
            "--BOUND\r\nContent-Disposition: form-data; name=\"payload_json\"\r\n\r\n{\"a\":1}\r\n"
        ));
        assert!(body.contains(
            "Content-Disposition: form-data; name=\"files[0]\"; filename=\"preview.png\"\r\n\
             Content-Type: image/png\r\n\r\n"
        ));
        assert!(body.ends_with("\r\n--BOUND--\r\n"));
    }

    #[test]
    fn response_classification_matches_fetch_semantics() {
        assert!(DeliveryResponse { status: 204 }.ok());
        assert!(!DeliveryResponse { status: 204 }.is_redirect());
        assert!(!DeliveryResponse { status: 302 }.ok());
        assert!(DeliveryResponse { status: 302 }.is_redirect());
        assert!(!DeliveryResponse { status: 500 }.ok());
    }

    #[test]
    fn the_timeout_is_the_reference_four_seconds() {
        assert_eq!(DELIVERY_TIMEOUT, Duration::from_millis(4000));
    }
}
