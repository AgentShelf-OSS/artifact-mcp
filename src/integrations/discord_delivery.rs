//! Discord's durable-delivery provider contract.
//!
//! This module deliberately does **not** enqueue work or alter the legacy notifier.  It is the
//! small, side-effect-free protocol boundary a later outbox worker can use to construct a safe
//! endpoint and turn an observed HTTP/transport result into an action.  The contract is bounded
//! at-least-once: retryable ambiguous outcomes can have been accepted by Discord, so duplicate
//! risk is always explicit rather than being mistaken for exactly-once delivery.

use std::{collections::BTreeMap, fmt, time::Duration};

use serde_json::Value;
use url::Url;

use crate::persistence::webhooks::is_discord_webhook_url;

/// The hard upper bound shared with the old notifier.  Provider callers must not widen it.
pub const DISCORD_DELIVERY_TIMEOUT: Duration = Duration::from_secs(4);
/// A Discord message snowflake is decimal.  The bound prevents a malformed success body becoming
/// an unbounded database/log value while leaving room for future snowflake widths.
pub const MAX_MESSAGE_ID_BYTES: usize = 32;
/// Provider response parsing is bounded before JSON decoding.
pub const MAX_RESPONSE_BODY_BYTES: usize = 65_536;
/// Header values retained as operational metadata are bounded too.
pub const MAX_RATE_LIMIT_VALUE_BYTES: usize = 128;
/// A malformed provider response must not poison the queue with an effectively permanent delay.
pub const MAX_RETRY_DELAY_MILLIS: u64 = 24 * 60 * 60 * 1_000;

/// The locked delivery semantic for the durable outbox.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeliverySemantics {
    /// Retry is bounded by the worker's attempt limit; an ambiguous retry can duplicate a post.
    BoundedAtLeastOnce,
}

/// The one semantic the outbox must persist with each provider outcome.
pub const DELIVERY_SEMANTICS: DeliverySemantics = DeliverySemantics::BoundedAtLeastOnce;

/// Whether retrying can create a second Discord message for one outbox event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DuplicateRisk {
    /// Discord definitively rejected the request before accepting a message (for example 429).
    None,
    /// The connection/result was ambiguous; the first post may already exist.
    Possible,
}

/// A parsed Discord rate-limit scope.  Unknown scopes are not invented or coerced.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RateLimitScope {
    Global,
    Shared,
    User,
}

/// The exact usable rate-limit metadata supplied by Discord, with source precedence documented in
/// [`classify_http_response`]. `retry_after_ms` is `None` only when Discord supplied no valid delay;
/// a worker must then not schedule a speculative 429 retry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RateLimitMetadata {
    /// Stable secret/outbox reference supplied by the worker, never a webhook URL.
    pub webhook_ref: Option<String>,
    pub bucket: Option<String>,
    pub remaining: Option<u64>,
    pub reset_after_ms: Option<u64>,
    pub retry_after_ms: Option<u64>,
    pub scope: Option<RateLimitScope>,
}

/// Retryable provider outcomes.  `Ambiguous` is deliberately distinct from ordinary 5xx retry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RetryReason {
    RateLimited,
    Network,
    Timeout,
    Ambiguous,
    ServerError,
}

/// Terminal outcomes are safe to dead-letter; none embeds a secret URL, token, or payload.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TerminalReason {
    InvalidSecret,
    DecryptFailed,
    AllowlistRejected,
    Redirect,
    BadRequest,
    Unauthorized,
    Forbidden,
    NotFound,
    InvalidRateLimitDelay,
    ClientError,
    ServerError,
}

/// The future worker's complete action for one provider attempt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeliveryClassification {
    /// A 2xx response whose bounded JSON body carried a valid Discord message id.
    Accepted {
        message_id: String,
        rate_limit: RateLimitMetadata,
    },
    /// The worker may retry, subject to its attempt bound.  For a 429, it may do so only after
    /// `rate_limit.retry_after_ms` is present; no invented exponential delay is permitted there.
    Retry {
        reason: RetryReason,
        duplicate_risk: DuplicateRisk,
        rate_limit: Option<RateLimitMetadata>,
    },
    /// The worker must stop retrying and dead-letter the event.
    Terminal { reason: TerminalReason },
}

/// Faults known before, during, or after the request.  Secret/decryption/allowlist failures stay
/// terminal, while a transport outcome that might conceal an accepted post stays explicitly risky.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProviderFault {
    InvalidSecret,
    DecryptFailed,
    AllowlistRejected,
    Network,
    Timeout,
    Ambiguous,
}

/// An observed response, deliberately stripped of request URL and payload.
#[derive(Clone, PartialEq, Eq)]
pub struct DiscordHttpResponse {
    pub status: u16,
    /// Opaque top-level webhook/secret reference. Discord bucket ids alone cannot serialize work.
    pub webhook_ref: Option<String>,
    pub headers: BTreeMap<String, String>,
    pub body: Vec<u8>,
}
impl fmt::Debug for DiscordHttpResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DiscordHttpResponse")
            .field("status", &self.status)
            .field("webhook_ref", &"<redacted>")
            .field("headers", &"<redacted>")
            .field("body", &"<redacted>")
            .finish()
    }
}

/// Build the execution endpoint without preserving arbitrary query parameters.
///
/// Existing safe Discord parameters `thread_id` and `with_components` survive;
/// all other parameters are dropped.  `wait=true` is always appended/replaced, and an explicit
/// `thread_id` replaces an existing one only when it is a bounded decimal Discord snowflake.
/// This prevents a malformed query from changing request semantics or smuggling a second target.
///
/// # Errors
/// Returns a fixed, secret-free error when the URL or supplied thread id is invalid.
pub fn execution_url(webhook_url: &str, thread_id: Option<&str>) -> Result<String, TerminalReason> {
    // Reuse the shipping raw-prefix guard first, then parse solely to normalize the query.
    if !is_discord_webhook_url(webhook_url) {
        return Err(TerminalReason::AllowlistRejected);
    }
    let mut url = Url::parse(webhook_url).map_err(|_| TerminalReason::AllowlistRejected)?;
    if url.fragment().is_some() {
        return Err(TerminalReason::AllowlistRejected);
    }
    let existing: Vec<(String, String)> = url.query_pairs().into_owned().collect();
    let mut thread = None;
    let mut components = None;
    for (key, value) in existing {
        match key.as_str() {
            "thread_id" if valid_snowflake(&value) && thread.is_none() => thread = Some(value),
            "with_components"
                if matches!(value.as_str(), "true" | "false") && components.is_none() =>
            {
                components = Some(value)
            }
            _ => {}
        }
    }
    if let Some(thread_id) = thread_id {
        if !valid_snowflake(thread_id) {
            return Err(TerminalReason::BadRequest);
        }
        thread = Some(thread_id.to_owned());
    }
    // `wait` is always exactly one, and always true.
    let mut query = url.query_pairs_mut();
    query.clear();
    if let Some(thread) = thread {
        query.append_pair("thread_id", &thread);
    }
    if let Some(components) = components {
        query.append_pair("with_components", &components);
    }
    query.append_pair("wait", "true");
    drop(query);
    let normalized: String = url.into();
    // Reapply the shared allowlist after changing query serialization.
    if !is_discord_webhook_url(&normalized) {
        return Err(TerminalReason::AllowlistRejected);
    }
    Ok(normalized)
}

/// Classify an error without ever deriving text from the URL, token, request body, or response.
#[must_use]
pub const fn classify_fault(fault: ProviderFault) -> DeliveryClassification {
    match fault {
        ProviderFault::InvalidSecret => DeliveryClassification::Terminal {
            reason: TerminalReason::InvalidSecret,
        },
        ProviderFault::DecryptFailed => DeliveryClassification::Terminal {
            reason: TerminalReason::DecryptFailed,
        },
        ProviderFault::AllowlistRejected => DeliveryClassification::Terminal {
            reason: TerminalReason::AllowlistRejected,
        },
        ProviderFault::Network => DeliveryClassification::Retry {
            reason: RetryReason::Network,
            duplicate_risk: DuplicateRisk::Possible,
            rate_limit: None,
        },
        ProviderFault::Timeout => DeliveryClassification::Retry {
            reason: RetryReason::Timeout,
            duplicate_risk: DuplicateRisk::Possible,
            rate_limit: None,
        },
        ProviderFault::Ambiguous => DeliveryClassification::Retry {
            reason: RetryReason::Ambiguous,
            duplicate_risk: DuplicateRisk::Possible,
            rate_limit: None,
        },
    }
}

/// Classify an HTTP response.  A 2xx is accepted **only** when a bounded JSON object contains a
/// valid `id` string.  Malformed/truncated 2xx bodies are ambiguous, because Discord may have
/// accepted the post before its response was damaged.
///
/// For a 429 the JSON `retry_after` and `retry-after` header are conservatively maxed;
/// `x-ratelimit-reset-after` remains metadata only. Bucket comes from `x-ratelimit-bucket`; scope is
/// body `global: true` before `x-ratelimit-scope`. Values are bounded, syntax-checked, and never
/// guessed. A missing/non-positive delay is terminal so generic backoff cannot violate the limit.
#[must_use]
pub fn classify_http_response(response: &DiscordHttpResponse) -> DeliveryClassification {
    match response.status {
        200..=299
            if !response
                .webhook_ref
                .as_deref()
                .is_some_and(valid_webhook_ref) =>
        {
            DeliveryClassification::Terminal {
                reason: TerminalReason::InvalidSecret,
            }
        }
        200..=299 => match message_id(&response.body) {
            Some(message_id) => DeliveryClassification::Accepted {
                message_id,
                rate_limit: rate_limit_metadata(response),
            },
            None => classify_fault(ProviderFault::Ambiguous),
        },
        300..=399 => DeliveryClassification::Terminal {
            reason: TerminalReason::Redirect,
        },
        400 => DeliveryClassification::Terminal {
            reason: TerminalReason::BadRequest,
        },
        401 => DeliveryClassification::Terminal {
            reason: TerminalReason::Unauthorized,
        },
        403 => DeliveryClassification::Terminal {
            reason: TerminalReason::Forbidden,
        },
        404 => DeliveryClassification::Terminal {
            reason: TerminalReason::NotFound,
        },
        429 => {
            let rate_limit = rate_limit_metadata(response);
            if !response
                .webhook_ref
                .as_deref()
                .is_some_and(valid_webhook_ref)
            {
                return DeliveryClassification::Terminal {
                    reason: TerminalReason::InvalidSecret,
                };
            }
            // The locked policy is *only* retry after a supplied positive Discord delay.  Do not
            // hand a `None`/zero delay to generic backoff: it would turn a rate-limit protocol
            // failure into an immediate, potentially abusive replay.
            if rate_limit.retry_after_ms.is_some_and(|delay| delay > 0) {
                DeliveryClassification::Retry {
                    reason: RetryReason::RateLimited,
                    duplicate_risk: DuplicateRisk::None,
                    rate_limit: Some(rate_limit),
                }
            } else {
                DeliveryClassification::Terminal {
                    reason: TerminalReason::InvalidRateLimitDelay,
                }
            }
        }
        500 | 502 | 503 | 504 => DeliveryClassification::Retry {
            reason: RetryReason::ServerError,
            duplicate_risk: DuplicateRisk::Possible,
            rate_limit: None,
        },
        405..=499 => DeliveryClassification::Terminal {
            reason: TerminalReason::ClientError,
        },
        500..=599 => DeliveryClassification::Terminal {
            reason: TerminalReason::ServerError,
        },
        _ => DeliveryClassification::Terminal {
            reason: TerminalReason::ClientError,
        },
    }
}

/// A request envelope with redacted `Debug`, so callers cannot accidentally trace credentials or
/// event payloads while wiring the worker.
#[derive(Clone, PartialEq, Eq)]
pub struct ProviderRequest {
    url: String,
    webhook_ref: String,
    content_type: String,
    payload: Vec<u8>,
}

impl fmt::Debug for ProviderRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderRequest")
            .field("url", &"<redacted>")
            .field("webhook_ref", &"<redacted>")
            .field("content_type", &self.content_type)
            .field("payload", &"<redacted>")
            .finish()
    }
}

/// Build a redaction-safe worker request.  The caller supplies serialized bytes; this API neither
/// logs nor parses the payload.
pub fn provider_request(
    webhook_url: &str,
    thread_id: Option<&str>,
    webhook_ref: String,
    content_type: String,
    payload: Vec<u8>,
) -> Result<ProviderRequest, TerminalReason> {
    if !valid_webhook_ref(&webhook_ref) {
        return Err(TerminalReason::InvalidSecret);
    }
    Ok(ProviderRequest {
        url: execution_url(webhook_url, thread_id)?,
        webhook_ref,
        content_type,
        payload,
    })
}

/// Production transport for the future outbox worker. It owns the redirect and four-second
/// enforcement rather than asking callers to honor advisory fields on [`ProviderRequest`].
pub struct DiscordProviderTransport {
    client: reqwest::Client,
    timeout: Duration,
}

impl DiscordProviderTransport {
    /// Build a redirect-refusing transport at the locked four-second timeout.
    ///
    /// # Errors
    /// Returns an opaque error when the HTTP client cannot be initialized.
    pub fn new() -> Result<Self, crate::error::AppError> {
        Self::with_timeout(DISCORD_DELIVERY_TIMEOUT)
    }

    /// Test-only/custom-bound constructor; requests can never configure redirects.
    ///
    /// # Errors
    /// As [`Self::new`].
    pub fn with_timeout(timeout: Duration) -> Result<Self, crate::error::AppError> {
        let timeout = timeout.min(DISCORD_DELIVERY_TIMEOUT);
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(timeout)
            .build()
            .map_err(|_| {
                crate::error::AppError::Unavailable("discord provider unavailable".to_owned())
            })?;
        Ok(Self { client, timeout })
    }

    /// Send and fully classify one provider attempt. The timeout encloses both request and bounded
    /// streamed body read: a body-read failure/timeout is ambiguous because Discord may have
    /// accepted the message before the response stream was lost.
    pub async fn deliver(&self, request: ProviderRequest) -> DeliveryClassification {
        let deadline = tokio::time::Instant::now() + self.timeout;
        let webhook_ref = request.webhook_ref.clone();
        if execution_url(&request.url, None).as_deref() != Ok(request.url.as_str()) {
            return classify_fault(ProviderFault::AllowlistRejected);
        }
        let send = self
            .client
            .post(&request.url)
            .header("content-type", request.content_type)
            .body(request.payload)
            .send();
        let response = match tokio::time::timeout(self.timeout, send).await {
            Err(_) => return classify_fault(ProviderFault::Timeout),
            Ok(Err(_)) => return classify_fault(ProviderFault::Network),
            Ok(Ok(response)) => response,
        };
        let status = response.status().as_u16();
        let headers: BTreeMap<String, String> = response
            .headers()
            .iter()
            .filter_map(|(name, value)| {
                value
                    .to_str()
                    .ok()
                    .map(|value| (name.to_string(), value.to_owned()))
            })
            .collect();
        let observed = DiscordHttpResponse {
            status,
            webhook_ref: Some(webhook_ref.clone()),
            headers: headers.clone(),
            body: Vec::new(),
        };
        if !(200..=299).contains(&status) && status != 429 {
            return classify_http_response(&observed);
        }
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let body = match tokio::time::timeout(remaining, read_response_body(response)).await {
            Ok(Ok(body)) => body,
            // A completed HTTP response with a bad/slow/oversized body is not proof of rejection.
            Ok(Err(())) | Err(_) if status == 429 => return classify_http_response(&observed),
            Ok(Err(())) | Err(_) => return classify_fault(ProviderFault::Ambiguous),
        };
        classify_http_response(&DiscordHttpResponse {
            status,
            webhook_ref: Some(webhook_ref),
            headers,
            body,
        })
    }
}

impl fmt::Debug for DiscordProviderTransport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DiscordProviderTransport")
            .field("timeout", &self.timeout)
            .finish_non_exhaustive()
    }
}

async fn read_response_body(mut response: reqwest::Response) -> Result<Vec<u8>, ()> {
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| ())? {
        if body.len().saturating_add(chunk.len()) > MAX_RESPONSE_BODY_BYTES {
            return Err(());
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn valid_snowflake(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_MESSAGE_ID_BYTES
        && !value.starts_with('0')
        && value.bytes().all(|byte| byte.is_ascii_digit())
}
fn valid_webhook_ref(value: &str) -> bool {
    let Some(suffix) = value
        .strip_prefix("webhook:")
        .or_else(|| value.strip_prefix("discussion:"))
    else {
        return false;
    };
    !suffix.is_empty()
        && value.len() <= MAX_RATE_LIMIT_VALUE_BYTES
        && suffix
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
}

fn message_id(body: &[u8]) -> Option<String> {
    if body.len() > MAX_RESPONSE_BODY_BYTES {
        return None;
    }
    let value: Value = serde_json::from_slice(body).ok()?;
    let id = value.get("id")?.as_str()?;
    valid_snowflake(id).then(|| id.to_owned())
}

fn rate_limit_metadata(response: &DiscordHttpResponse) -> RateLimitMetadata {
    let body = (response.body.len() <= MAX_RESPONSE_BODY_BYTES)
        .then(|| serde_json::from_slice::<Value>(&response.body).ok())
        .flatten();
    let body_retry_after = body
        .as_ref()
        .and_then(|value| value.get("retry_after"))
        .and_then(delay_millis_from_json);
    let retry_after_header = header(response, "retry-after").and_then(delay_millis_from_text);
    // Both the JSON body and Retry-After are supplied constraints. On disagreement, waiting for
    // the maximum is conservative; Reset-After remains metadata only.
    let retry_after_ms = match (body_retry_after, retry_after_header) {
        (Some(body), Some(header)) => Some(body.max(header)),
        (Some(delay), None) | (None, Some(delay)) => Some(delay),
        (None, None) => None,
    };
    let bucket = header(response, "x-ratelimit-bucket")
        .filter(|value| bounded_header(value))
        .map(ToOwned::to_owned);
    let scope = if body
        .as_ref()
        .and_then(|value| value.get("global"))
        .and_then(Value::as_bool)
        == Some(true)
        || header(response, "x-ratelimit-global") == Some("true")
    {
        Some(RateLimitScope::Global)
    } else {
        header(response, "x-ratelimit-scope").and_then(parse_scope)
    };
    let remaining = header(response, "x-ratelimit-remaining")
        .filter(|value| bounded_header(value))
        .and_then(|value| value.parse::<u64>().ok());
    RateLimitMetadata {
        webhook_ref: response.webhook_ref.clone(),
        bucket,
        remaining,
        reset_after_ms: header(response, "x-ratelimit-reset-after")
            .and_then(delay_millis_from_text),
        retry_after_ms,
        scope,
    }
}

fn header<'a>(response: &'a DiscordHttpResponse, wanted: &str) -> Option<&'a str> {
    response
        .headers
        .iter()
        .find_map(|(name, value)| name.eq_ignore_ascii_case(wanted).then_some(value.as_str()))
}

fn bounded_header(value: &str) -> bool {
    !value.is_empty() && value.len() <= MAX_RATE_LIMIT_VALUE_BYTES && !value.contains(['\r', '\n'])
}

fn parse_scope(value: &str) -> Option<RateLimitScope> {
    match value {
        "global" => Some(RateLimitScope::Global),
        "shared" => Some(RateLimitScope::Shared),
        "user" => Some(RateLimitScope::User),
        _ => None,
    }
}

fn delay_millis_from_json(value: &Value) -> Option<u64> {
    value.as_f64().and_then(delay_millis_from_seconds)
}

fn delay_millis_from_text(value: &str) -> Option<u64> {
    strict_decimal_seconds(value)
        .then(|| value.parse::<f64>().ok())
        .flatten()
        .and_then(delay_millis_from_seconds)
}

fn delay_millis_from_seconds(seconds: f64) -> Option<u64> {
    if !seconds.is_finite() || seconds <= 0.0 {
        return None;
    }
    let milliseconds = (seconds * 1_000.0).ceil();
    (milliseconds.is_finite()
        && milliseconds >= 1.0
        && milliseconds <= MAX_RETRY_DELAY_MILLIS as f64)
        .then_some(milliseconds as u64)
}

fn strict_decimal_seconds(value: &str) -> bool {
    let mut parts = value.split('.');
    let Some(whole) = parts.next() else {
        return false;
    };
    let fraction = parts.next();
    !whole.is_empty()
        && whole.bytes().all(|byte| byte.is_ascii_digit())
        && fraction.is_none_or(|digits| {
            !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())
        })
        && parts.next().is_none()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response(status: u16, headers: &[(&str, &str)], body: &str) -> DiscordHttpResponse {
        DiscordHttpResponse {
            status,
            webhook_ref: Some("webhook:wh-ref".to_owned()),
            headers: headers
                .iter()
                .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
                .collect(),
            body: body.as_bytes().to_vec(),
        }
    }

    #[test]
    fn wait_is_replaced_and_safe_thread_parameters_survive() {
        assert_eq!(
            execution_url(
                "https://discord.com/api/webhooks/1/token?thread_id=12&wait=false&junk=x&with_components=true",
                Some("34"),
            ),
            Ok("https://discord.com/api/webhooks/1/token?thread_id=34&with_components=true&wait=true".to_owned())
        );
    }

    #[test]
    fn only_a_bounded_message_id_makes_2xx_accepted() {
        assert_eq!(
            classify_http_response(&response(200, &[], r#"{"id":"123456789012345678"}"#)),
            DeliveryClassification::Accepted {
                message_id: "123456789012345678".to_owned(),
                rate_limit: RateLimitMetadata {
                    webhook_ref: Some("webhook:wh-ref".to_owned()),
                    bucket: None,
                    remaining: None,
                    reset_after_ms: None,
                    retry_after_ms: None,
                    scope: None,
                },
            }
        );
        for body in [
            r#"{}"#,
            r#"{"id":12}"#,
            r#"{"id":"001"}"#,
            r#"{"id":"abc"}"#,
        ] {
            assert_eq!(
                classify_http_response(&response(204, &[], body)),
                classify_fault(ProviderFault::Ambiguous)
            );
        }
    }

    #[test]
    fn rate_limit_uses_exact_body_delay_then_headers_and_scope() {
        let outcome = classify_http_response(&response(
            429,
            &[
                ("X-RateLimit-Reset-After", "9"),
                ("X-RateLimit-Bucket", "bucket-a"),
                ("X-RateLimit-Scope", "shared"),
            ],
            r#"{"retry_after":0.25,"global":true}"#,
        ));
        assert_eq!(
            outcome,
            DeliveryClassification::Retry {
                reason: RetryReason::RateLimited,
                duplicate_risk: DuplicateRisk::None,
                rate_limit: Some(RateLimitMetadata {
                    webhook_ref: Some("webhook:wh-ref".to_owned()),
                    bucket: Some("bucket-a".to_owned()),
                    remaining: None,
                    reset_after_ms: Some(9_000),
                    retry_after_ms: Some(250),
                    scope: Some(RateLimitScope::Global),
                }),
            }
        );
    }

    #[test]
    fn a_rate_limit_without_a_positive_supplied_delay_is_terminal_not_generic_backoff() {
        assert_eq!(
            classify_http_response(&response(429, &[], "{}")),
            DeliveryClassification::Terminal {
                reason: TerminalReason::InvalidRateLimitDelay,
            }
        );
    }

    #[test]
    fn request_debug_and_all_outcomes_are_secret_free() {
        let request = provider_request(
            "https://discord.com/api/webhooks/1/SUPER-SECRET",
            None,
            "webhook:secret-ref".to_owned(),
            "application/json".to_owned(),
            b"{\"secret\":true}".to_vec(),
        )
        .expect("valid request");
        let debug = format!("{request:?}");
        assert!(!debug.contains("SUPER-SECRET"));
        assert!(!debug.contains("secret"));
    }

    #[tokio::test]
    async fn transport_uses_one_deadline_for_send_and_body_read() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind(("127.0.0.1", 0)).await.expect("bind");
        let address = listener.local_addr().expect("address");
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let mut request = [0_u8; 1024];
            let _ = socket.read(&mut request).await;
            tokio::time::sleep(Duration::from_millis(70)).await;
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\nConnection: close\r\n\r\n")
                .await
                .expect("headers");
            tokio::time::sleep(Duration::from_secs(1)).await;
        });
        let transport =
            DiscordProviderTransport::with_timeout(Duration::from_millis(100)).expect("transport");
        let started = std::time::Instant::now();
        let outcome = transport
            .deliver(ProviderRequest {
                url: format!("http://{address}/api/webhooks/1/t"),
                webhook_ref: "webhook:test-ref".to_owned(),
                content_type: "application/json".to_owned(),
                payload: b"{}".to_vec(),
            })
            .await;
        assert_eq!(outcome, classify_fault(ProviderFault::AllowlistRejected));
        assert!(
            started.elapsed() < Duration::from_millis(160),
            "send and body each received a full timeout: {:?}",
            started.elapsed()
        );
    }
}
