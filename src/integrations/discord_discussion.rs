//! Discord discussion transport for legacy Forum/Media posts and notification-anchored threads.
//!
//! The public operation and result types are deliberately provider-neutral: PBI-079's
//! orchestration can persist one artifact discussion contract without treating a Discord URL as
//! an identifier.  This adapter only converts the operation to Discord's webhook protocol.  It
//! neither owns an outbox row nor performs a database operation.
//!
//! Every request uses one redirect-refusing deadline and `allowed_mentions` with an empty parse
//! list.  `Debug` for all secret- or comment-bearing values is hand-written and redacted.

use std::{collections::BTreeMap, fmt, time::Duration};

use reqwest::Method;
use serde_json::{Value, json};
use url::Url;

use crate::{
    config::Secret,
    error::AppError,
    integrations::discord_delivery::{
        DISCORD_DELIVERY_TIMEOUT, DiscordHttpResponse, DuplicateRisk, ProviderFault,
        RateLimitMetadata, RetryReason, TerminalReason, classify_fault, classify_http_response,
    },
    persistence::webhooks::is_discord_webhook_url,
};

/// Discord limits normal message content to 2,000 characters.
pub const MAX_DISCUSSION_CONTENT_CHARS: usize = 2_000;
/// Discord forum and media posts have a bounded thread name.
pub const MAX_THREAD_NAME_CHARS: usize = 100;
/// Discord snowflakes are decimal and this bound matches PBI-056 delivery IDs.
pub const MAX_DISCUSSION_ID_BYTES: usize = 32;
const MAX_RESPONSE_BODY_BYTES: usize = 65_536;
const DISCORD_USER_AGENT: &str = "Artifact-MCP (discord-thread-bridge)";

/// Safe metadata returned by Discord's token-authenticated incoming-webhook lookup.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscordWebhookDestination {
    pub webhook_id: String,
    pub channel_id: String,
    pub guild_id: String,
}

/// A discussion action independently meaningful to a future provider adapter.
///
/// The text fields are intentionally private to this module's debug formatter.  Callers create
/// an action through the constructors so invalid/unbounded values cannot reach a webhook.
#[derive(Clone, PartialEq, Eq)]
pub enum DiscussionOperation {
    CreateThread {
        thread_name: String,
        content: String,
    },
    CreateThreadFromMessage {
        channel_id: String,
        message_id: String,
        thread_name: String,
        content: String,
    },
    Reply {
        thread_id: String,
        content: String,
    },
    ResolvedMarker {
        thread_id: String,
    },
    ReopenedMarker {
        thread_id: String,
    },
    Tombstone {
        thread_id: String,
        message_id: String,
    },
}

impl fmt::Debug for DiscussionOperation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CreateThread { .. } => formatter
                .debug_struct("CreateThread")
                .field("thread_name", &"<redacted>")
                .field("content", &"<redacted>")
                .finish(),
            Self::CreateThreadFromMessage {
                channel_id,
                message_id,
                ..
            } => formatter
                .debug_struct("CreateThreadFromMessage")
                .field("channel_id", channel_id)
                .field("message_id", message_id)
                .field("thread_name", &"<redacted>")
                .field("content", &"<redacted>")
                .finish(),
            Self::Reply { thread_id, .. } => formatter
                .debug_struct("Reply")
                .field("thread_id", thread_id)
                .field("content", &"<redacted>")
                .finish(),
            Self::ResolvedMarker { thread_id } => formatter
                .debug_struct("ResolvedMarker")
                .field("thread_id", thread_id)
                .finish(),
            Self::ReopenedMarker { thread_id } => formatter
                .debug_struct("ReopenedMarker")
                .field("thread_id", thread_id)
                .finish(),
            Self::Tombstone {
                thread_id,
                message_id,
            } => formatter
                .debug_struct("Tombstone")
                .field("thread_id", thread_id)
                .field("message_id", message_id)
                .finish(),
        }
    }
}

impl DiscussionOperation {
    /// Build the first mirrored comment and its Discord forum/media post name.
    pub fn create_thread(thread_name: String, content: String) -> Result<Self, TerminalReason> {
        validate_thread_name(&thread_name)?;
        validate_content(&content)?;
        Ok(Self::CreateThread {
            thread_name,
            content,
        })
    }

    /// Build the first mirrored comment in a public thread attached to an existing message.
    pub fn create_thread_from_message(
        channel_id: String,
        message_id: String,
        thread_name: String,
        content: String,
    ) -> Result<Self, TerminalReason> {
        validate_id(&channel_id)?;
        validate_id(&message_id)?;
        validate_thread_name(&thread_name)?;
        validate_content(&content)?;
        Ok(Self::CreateThreadFromMessage {
            channel_id,
            message_id,
            thread_name,
            content,
        })
    }

    /// Build a later comment or reply for the existing external thread.
    pub fn reply(thread_id: String, content: String) -> Result<Self, TerminalReason> {
        validate_id(&thread_id)?;
        validate_content(&content)?;
        Ok(Self::Reply { thread_id, content })
    }

    /// Append the compact, system-authored resolved marker.
    pub fn resolved_marker(thread_id: String) -> Result<Self, TerminalReason> {
        validate_id(&thread_id)?;
        Ok(Self::ResolvedMarker { thread_id })
    }

    /// Append the compact, system-authored reopened marker.
    pub fn reopened_marker(thread_id: String) -> Result<Self, TerminalReason> {
        validate_id(&thread_id)?;
        Ok(Self::ReopenedMarker { thread_id })
    }

    /// Replace a webhook-authored comment with a compact deletion tombstone.
    pub fn tombstone(thread_id: String, message_id: String) -> Result<Self, TerminalReason> {
        validate_id(&thread_id)?;
        validate_id(&message_id)?;
        Ok(Self::Tombstone {
            thread_id,
            message_id,
        })
    }

    fn name(&self) -> &'static str {
        match self {
            Self::CreateThread { .. } => "create_thread",
            Self::CreateThreadFromMessage { .. } => "create_thread_from_message",
            Self::Reply { .. } => "reply",
            Self::ResolvedMarker { .. } => "resolved_marker",
            Self::ReopenedMarker { .. } => "reopened_marker",
            Self::Tombstone { .. } => "tombstone",
        }
    }

    fn requires_thread_receipt(&self) -> bool {
        matches!(
            self,
            Self::CreateThread { .. } | Self::CreateThreadFromMessage { .. }
        )
    }
}

/// Provider-neutral acceptance receipt. `thread_id` is present only after creating a forum/media
/// post; later replies intentionally retain their already persisted thread mapping.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscussionReceipt {
    pub message_id: String,
    pub thread_id: Option<String>,
}

/// Provider-neutral terminal/retry outcome, sharing PBI-056's locked semantics and rate-limit
/// metadata.  It contains no Discord URL, credential, or user-authored content.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DiscussionResult {
    Accepted {
        receipt: DiscussionReceipt,
        rate_limit: RateLimitMetadata,
    },
    Retry {
        reason: RetryReason,
        duplicate_risk: DuplicateRisk,
        rate_limit: Option<RateLimitMetadata>,
    },
    Terminal {
        reason: TerminalReason,
    },
}

/// A redaction-safe executable request. The webhook URL and comment body remain private.
#[derive(Clone, PartialEq, Eq)]
pub struct DiscussionRequest {
    url: String,
    webhook_ref: String,
    method: Method,
    payload: Vec<u8>,
    operation: DiscussionOperation,
    thread_anchor: Option<ThreadAnchorRequest>,
}

#[derive(Clone, PartialEq, Eq)]
struct ThreadAnchorRequest {
    create_url: String,
    probe_url: String,
    payload: Vec<u8>,
    thread_id: String,
    parent_channel_id: String,
}

impl fmt::Debug for DiscussionRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DiscussionRequest")
            .field("url", &"<redacted>")
            .field("webhook_ref", &"<redacted>")
            .field("method", &self.method)
            .field("payload", &"<redacted>")
            .field("operation", &self.operation.name())
            .field("thread_anchor", &self.thread_anchor.is_some())
            .finish()
    }
}

/// Build a Discord-backed request for a provider-neutral discussion operation.
///
/// `webhook_ref` is the opaque durable secret reference, not a URL.  It is validated before a
/// request can be constructed, while the URL is checked against the shared HTTPS Discord
/// allowlist and normalised to discard caller-supplied query parameters.
pub fn discussion_request(
    webhook_url: &str,
    webhook_ref: String,
    operation: DiscussionOperation,
) -> Result<DiscussionRequest, TerminalReason> {
    discussion_request_inner(webhook_url, webhook_ref, operation, None)
}

/// Redirect-refusing Discord implementation of the discussion transport.
pub struct DiscordDiscussionTransport {
    client: reqwest::Client,
    timeout: Duration,
    bot_token: Option<Secret>,
}

impl DiscordDiscussionTransport {
    /// Build the production transport with PBI-056's four-second end-to-end deadline.
    ///
    /// # Errors
    /// Returns an opaque availability error when the HTTP client cannot be initialized.
    pub fn new() -> Result<Self, crate::error::AppError> {
        Self::with_timeout_and_bot_token(DISCORD_DELIVERY_TIMEOUT, None)
    }

    /// Build the production transport with the optional bot credential used only for thread
    /// management. Legacy Forum/Media delivery remains available without a token.
    pub fn with_bot_token(bot_token: Option<Secret>) -> Result<Self, crate::error::AppError> {
        Self::with_timeout_and_bot_token(DISCORD_DELIVERY_TIMEOUT, bot_token)
    }

    /// Build a transport with a tighter test bound. The upper production limit cannot be widened.
    ///
    /// # Errors
    /// As [`Self::new`].
    pub fn with_timeout(timeout: Duration) -> Result<Self, crate::error::AppError> {
        Self::with_timeout_and_bot_token(timeout, None)
    }

    fn with_timeout_and_bot_token(
        timeout: Duration,
        bot_token: Option<Secret>,
    ) -> Result<Self, crate::error::AppError> {
        let timeout = timeout.min(DISCORD_DELIVERY_TIMEOUT);
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(timeout)
            .build()
            .map_err(|_| {
                crate::error::AppError::Unavailable("discord discussion unavailable".to_owned())
            })?;
        Ok(Self {
            client,
            timeout,
            bot_token,
        })
    }

    /// Send one bounded provider attempt. The transport never follows redirects, and the single
    /// deadline includes response-body parsing. A damaged success body is ambiguous because the
    /// message may already have been accepted by Discord.
    pub async fn deliver(&self, request: DiscussionRequest) -> DiscussionResult {
        self.deliver_inner(request).await
    }

    /// Whether notification-anchored threads can be configured in this process.
    #[must_use]
    pub const fn bot_configured(&self) -> bool {
        self.bot_token.is_some()
    }

    /// Validate only the bot identity when an organization has not selected its destination yet.
    /// Enabling outbound or inbound behavior still requires a later exact destination check.
    pub async fn validate_bot_token(&self) -> Result<(), AppError> {
        let Some(token) = self.bot_token.as_ref() else {
            return Err(AppError::Validation(
                "A Discord bot credential is required.".to_owned(),
            ));
        };
        let response = tokio::time::timeout(
            self.timeout,
            self.client
                .get("https://discord.com/api/v10/users/@me")
                .header("authorization", format!("Bot {}", token.expose()))
                .header("user-agent", DISCORD_USER_AGENT)
                .send(),
        )
        .await
        .map_err(|_| AppError::Unavailable("discord bot validation unavailable".to_owned()))?
        .map_err(|_| AppError::Unavailable("discord bot validation unavailable".to_owned()))?;
        if !response.status().is_success() {
            return Err(AppError::Validation(
                "Discord bot credential validation failed.".to_owned(),
            ));
        }
        let body = read_response_body(response)
            .await
            .map_err(|_| AppError::Unavailable("discord bot validation unavailable".to_owned()))?;
        let value: Value = serde_json::from_slice(&body)
            .map_err(|_| AppError::Unavailable("discord bot validation unavailable".to_owned()))?;
        if value.get("id").and_then(Value::as_str).is_none()
            || value.get("bot").and_then(Value::as_bool) != Some(true)
        {
            return Err(AppError::Validation(
                "Discord bot credential validation failed.".to_owned(),
            ));
        }
        Ok(())
    }

    /// Resolve the selected incoming webhook's channel and guild, then verify that the configured
    /// bot can see a supported text/announcement channel. No credential enters the returned value
    /// or an error.
    pub async fn inspect_notification_webhook(
        &self,
        webhook_url: &str,
    ) -> Result<DiscordWebhookDestination, AppError> {
        let Some(token) = self.bot_token.as_ref() else {
            return Err(AppError::Validation(
                "DISCORD_BOT_TOKEN is required for notification threads.".to_owned(),
            ));
        };
        let mut webhook = parse_allowed_webhook(webhook_url, None)
            .map_err(|_| AppError::Validation("Invalid Discord webhook.".to_owned()))?;
        webhook.set_query(None);
        let response = tokio::time::timeout(self.timeout, self.client.get(webhook).send())
            .await
            .map_err(|_| {
                AppError::Unavailable("discord notification destination unavailable".to_owned())
            })?
            .map_err(|_| {
                AppError::Unavailable("discord notification destination unavailable".to_owned())
            })?;
        if !response.status().is_success() {
            return Err(AppError::Validation(
                "The selected Discord webhook is unavailable.".to_owned(),
            ));
        }
        let body = read_response_body(response).await.map_err(|_| {
            AppError::Unavailable("discord notification destination unavailable".to_owned())
        })?;
        let destination = parse_webhook_destination(&body).ok_or_else(|| {
            AppError::Validation(
                "The selected Discord webhook has no channel destination.".to_owned(),
            )
        })?;
        let channel_url = format!(
            "https://discord.com/api/v10/channels/{}",
            destination.channel_id
        );
        let response = tokio::time::timeout(
            self.timeout,
            self.client
                .get(channel_url)
                .header("authorization", format!("Bot {}", token.expose()))
                .header("user-agent", "Artifact-MCP (discord-thread-bridge)")
                .send(),
        )
        .await
        .map_err(|_| AppError::Unavailable("discord bot channel check unavailable".to_owned()))?
        .map_err(|_| AppError::Unavailable("discord bot channel check unavailable".to_owned()))?;
        if !response.status().is_success() {
            return Err(AppError::Validation(
                "The Discord bot cannot access the selected webhook channel.".to_owned(),
            ));
        }
        let body = read_response_body(response).await.map_err(|_| {
            AppError::Unavailable("discord bot channel check unavailable".to_owned())
        })?;
        if !valid_parent_channel(&body, &destination.channel_id) {
            return Err(AppError::Validation(
                "Select a Discord text or announcement channel for artifact notifications."
                    .to_owned(),
            ));
        }
        Ok(destination)
    }

    /// Post a visible parent test notification, create its public thread with the bot, and place a
    /// webhook-authored confirmation in that thread.
    pub async fn test_notification_thread(
        &self,
        webhook_url: &str,
        webhook_ref: &str,
        channel_id: &str,
        label: &str,
    ) -> bool {
        if !valid_webhook_ref(webhook_ref) || validate_id(channel_id).is_err() {
            return false;
        }
        let url = match execute_url(webhook_url, None, None) {
            Ok(url) => url,
            Err(_) => return false,
        };
        let payload = message_payload(
            "[Artifact MCP] Notification-thread connection test. A thread should appear on this message.",
            None,
        );
        let response = match tokio::time::timeout(
            self.timeout,
            self.client
                .post(url)
                .header("content-type", "application/json")
                .body(payload)
                .send(),
        )
        .await
        {
            Ok(Ok(response)) => response,
            _ => return false,
        };
        let status = response.status().as_u16();
        let headers = response_headers(&response);
        let body = match read_response_body(response).await {
            Ok(body) => body,
            Err(()) => return false,
        };
        let message_id = match classify_http_response(&DiscordHttpResponse {
            status,
            webhook_ref: Some(webhook_ref.to_owned()),
            headers,
            body,
        }) {
            crate::integrations::discord_delivery::DeliveryClassification::Accepted {
                message_id,
                ..
            } => message_id,
            _ => return false,
        };
        let name = if label.trim().is_empty() {
            "Artifact MCP connection test".to_owned()
        } else {
            format!("{} connection test", label.trim())
        };
        let operation = match DiscussionOperation::create_thread_from_message(
            channel_id.to_owned(),
            message_id,
            name.chars().take(MAX_THREAD_NAME_CHARS).collect(),
            "Artifact MCP can mirror comments into this notification thread. Discord replies remain one-way for this pilot.".to_owned(),
        ) {
            Ok(operation) => operation,
            Err(_) => return false,
        };
        let request = match discussion_request(webhook_url, webhook_ref.to_owned(), operation) {
            Ok(request) => request,
            Err(_) => return false,
        };
        matches!(
            self.deliver(request).await,
            DiscussionResult::Accepted { .. }
        )
    }

    async fn deliver_inner(&self, request: DiscussionRequest) -> DiscussionResult {
        let deadline = tokio::time::Instant::now() + self.timeout;
        if let Some(anchor) = request.thread_anchor.as_ref()
            && let Err(result) = self
                .ensure_notification_thread(anchor, &request.webhook_ref, deadline)
                .await
        {
            return result;
        }
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return result_from_delivery(classify_fault(ProviderFault::Timeout));
        }
        let send = self
            .client
            .request(request.method.clone(), &request.url)
            .header("content-type", "application/json")
            .header("user-agent", DISCORD_USER_AGENT)
            .body(request.payload)
            .send();
        let response = match tokio::time::timeout(remaining, send).await {
            Err(_) => return result_from_delivery(classify_fault(ProviderFault::Timeout)),
            Ok(Err(error)) if error.is_timeout() => {
                return result_from_delivery(classify_fault(ProviderFault::Timeout));
            }
            Ok(Err(_)) => return result_from_delivery(classify_fault(ProviderFault::Network)),
            Ok(Ok(response)) => response,
        };
        let status = response.status().as_u16();
        let headers = response_headers(&response);
        let observed = DiscordHttpResponse {
            status,
            webhook_ref: Some(request.webhook_ref.clone()),
            headers: headers.clone(),
            body: Vec::new(),
        };
        if !(200..=299).contains(&status) && status != 429 {
            return result_from_delivery(classify_http_response(&observed));
        }
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let body = match tokio::time::timeout(remaining, read_response_body(response)).await {
            Ok(Ok(body)) => body,
            Ok(Err(())) | Err(_) if status == 429 => {
                return result_from_delivery(classify_http_response(&observed));
            }
            Ok(Err(())) | Err(_) => {
                return result_from_delivery(classify_fault(ProviderFault::Ambiguous));
            }
        };
        let classified = classify_http_response(&DiscordHttpResponse {
            status,
            webhook_ref: Some(request.webhook_ref),
            headers,
            body: body.clone(),
        });
        match classified {
            crate::integrations::discord_delivery::DeliveryClassification::Accepted {
                message_id,
                rate_limit,
            } => {
                let thread_id = if let Some(anchor) = request.thread_anchor {
                    Some(anchor.thread_id)
                } else if request.operation.requires_thread_receipt() {
                    match response_thread_id(&body) {
                        Some(id) => Some(id),
                        None => {
                            return result_from_delivery(classify_fault(ProviderFault::Ambiguous));
                        }
                    }
                } else {
                    None
                };
                DiscussionResult::Accepted {
                    receipt: DiscussionReceipt {
                        message_id,
                        thread_id,
                    },
                    rate_limit,
                }
            }
            other => result_from_delivery(other),
        }
    }

    async fn ensure_notification_thread(
        &self,
        anchor: &ThreadAnchorRequest,
        webhook_ref: &str,
        deadline: tokio::time::Instant,
    ) -> Result<(), DiscussionResult> {
        let Some(token) = self.bot_token.as_ref() else {
            return Err(DiscussionResult::Terminal {
                reason: TerminalReason::InvalidSecret,
            });
        };
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err(result_from_delivery(classify_fault(ProviderFault::Timeout)));
        }
        let send = self
            .client
            .post(&anchor.create_url)
            .header("authorization", format!("Bot {}", token.expose()))
            .header("content-type", "application/json")
            .header("user-agent", DISCORD_USER_AGENT)
            .body(anchor.payload.clone())
            .send();
        let response = match tokio::time::timeout(remaining, send).await {
            Err(_) => {
                return Err(result_from_delivery(classify_fault(ProviderFault::Timeout)));
            }
            Ok(Err(error)) if error.is_timeout() => {
                return Err(result_from_delivery(classify_fault(ProviderFault::Timeout)));
            }
            Ok(Err(_)) => {
                return Err(result_from_delivery(classify_fault(ProviderFault::Network)));
            }
            Ok(Ok(response)) => response,
        };
        let status = response.status().as_u16();
        let headers = response_headers(&response);
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let body = match tokio::time::timeout(remaining, read_response_body(response)).await {
            Ok(Ok(body)) => body,
            Ok(Err(())) | Err(_) => {
                return Err(result_from_delivery(classify_fault(
                    ProviderFault::Ambiguous,
                )));
            }
        };
        if (200..=299).contains(&status)
            && valid_thread_channel(&body, &anchor.thread_id, &anchor.parent_channel_id)
        {
            return Ok(());
        }
        if (200..=299).contains(&status) || matches!(status, 400 | 409) {
            let exists = self
                .probe_notification_thread(anchor, webhook_ref, deadline)
                .await?;
            if exists {
                return Ok(());
            }
            return if (200..=299).contains(&status) {
                Err(result_from_delivery(classify_fault(
                    ProviderFault::Ambiguous,
                )))
            } else {
                Err(classify_bot_http(status, headers, body, webhook_ref))
            };
        }
        Err(classify_bot_http(status, headers, body, webhook_ref))
    }

    async fn probe_notification_thread(
        &self,
        anchor: &ThreadAnchorRequest,
        webhook_ref: &str,
        deadline: tokio::time::Instant,
    ) -> Result<bool, DiscussionResult> {
        let Some(token) = self.bot_token.as_ref() else {
            return Err(DiscussionResult::Terminal {
                reason: TerminalReason::InvalidSecret,
            });
        };
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err(result_from_delivery(classify_fault(ProviderFault::Timeout)));
        }
        let send = self
            .client
            .get(&anchor.probe_url)
            .header("authorization", format!("Bot {}", token.expose()))
            .header("user-agent", DISCORD_USER_AGENT)
            .send();
        let response = match tokio::time::timeout(remaining, send).await {
            Err(_) => {
                return Err(result_from_delivery(classify_fault(ProviderFault::Timeout)));
            }
            Ok(Err(error)) if error.is_timeout() => {
                return Err(result_from_delivery(classify_fault(ProviderFault::Timeout)));
            }
            Ok(Err(_)) => {
                return Err(result_from_delivery(classify_fault(ProviderFault::Network)));
            }
            Ok(Ok(response)) => response,
        };
        let status = response.status().as_u16();
        let headers = response_headers(&response);
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let body = match tokio::time::timeout(remaining, read_response_body(response)).await {
            Ok(Ok(body)) => body,
            Ok(Err(())) | Err(_) => {
                return Err(result_from_delivery(classify_fault(
                    ProviderFault::Ambiguous,
                )));
            }
        };
        if (200..=299).contains(&status) {
            return if valid_thread_channel(&body, &anchor.thread_id, &anchor.parent_channel_id) {
                Ok(true)
            } else {
                Err(result_from_delivery(classify_fault(
                    ProviderFault::Ambiguous,
                )))
            };
        }
        if status == 404 {
            return Ok(false);
        }
        Err(classify_bot_http(status, headers, body, webhook_ref))
    }
}

impl fmt::Debug for DiscordDiscussionTransport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DiscordDiscussionTransport")
            .field("timeout", &self.timeout)
            .field("bot_configured", &self.bot_token.is_some())
            .finish_non_exhaustive()
    }
}

fn discussion_request_inner(
    webhook_url: &str,
    webhook_ref: String,
    operation: DiscussionOperation,
    #[allow(unused_variables)] test_origin: Option<&str>,
) -> Result<DiscussionRequest, TerminalReason> {
    if !valid_webhook_ref(&webhook_ref) {
        return Err(TerminalReason::InvalidSecret);
    }
    validate_operation(&operation)?;
    let (method, url, payload, thread_anchor) = match &operation {
        DiscussionOperation::CreateThread {
            thread_name,
            content,
        } => (
            Method::POST,
            execute_url(webhook_url, None, test_origin)?,
            message_payload(content, Some(thread_name)),
            None,
        ),
        DiscussionOperation::CreateThreadFromMessage {
            channel_id,
            message_id,
            thread_name,
            content,
        } => (
            Method::POST,
            execute_url(webhook_url, Some(message_id), test_origin)?,
            message_payload(content, None),
            Some(thread_anchor_request(
                channel_id,
                message_id,
                thread_name,
                test_origin,
            )?),
        ),
        DiscussionOperation::Reply { thread_id, content } => (
            Method::POST,
            execute_url(webhook_url, Some(thread_id), test_origin)?,
            message_payload(content, None),
            None,
        ),
        DiscussionOperation::ResolvedMarker { thread_id } => (
            Method::POST,
            execute_url(webhook_url, Some(thread_id), test_origin)?,
            marker_payload("Discussion resolved."),
            None,
        ),
        DiscussionOperation::ReopenedMarker { thread_id } => (
            Method::POST,
            execute_url(webhook_url, Some(thread_id), test_origin)?,
            marker_payload("Discussion reopened."),
            None,
        ),
        DiscussionOperation::Tombstone {
            thread_id,
            message_id,
        } => (
            Method::PATCH,
            webhook_message_url(webhook_url, thread_id, message_id, test_origin)?,
            marker_payload("This Artifact MCP comment was deleted."),
            None,
        ),
    };
    Ok(DiscussionRequest {
        url,
        webhook_ref,
        method,
        payload,
        operation,
        thread_anchor,
    })
}

fn validate_operation(operation: &DiscussionOperation) -> Result<(), TerminalReason> {
    match operation {
        DiscussionOperation::CreateThread {
            thread_name,
            content,
        } => {
            validate_thread_name(thread_name)?;
            validate_content(content)
        }
        DiscussionOperation::CreateThreadFromMessage {
            channel_id,
            message_id,
            thread_name,
            content,
        } => {
            validate_id(channel_id)?;
            validate_id(message_id)?;
            validate_thread_name(thread_name)?;
            validate_content(content)
        }
        DiscussionOperation::Reply { thread_id, content } => {
            validate_id(thread_id)?;
            validate_content(content)
        }
        DiscussionOperation::ResolvedMarker { thread_id }
        | DiscussionOperation::ReopenedMarker { thread_id } => validate_id(thread_id),
        DiscussionOperation::Tombstone {
            thread_id,
            message_id,
        } => {
            validate_id(thread_id)?;
            validate_id(message_id)
        }
    }
}

fn thread_anchor_request(
    channel_id: &str,
    message_id: &str,
    thread_name: &str,
    test_origin: Option<&str>,
) -> Result<ThreadAnchorRequest, TerminalReason> {
    validate_id(channel_id)?;
    validate_id(message_id)?;
    validate_thread_name(thread_name)?;
    let api_origin = test_origin
        .map(str::trim_end)
        .unwrap_or("https://discord.com")
        .trim_end_matches('/');
    let create_url =
        format!("{api_origin}/api/v10/channels/{channel_id}/messages/{message_id}/threads");
    let probe_url = format!("{api_origin}/api/v10/channels/{message_id}");
    Ok(ThreadAnchorRequest {
        create_url,
        probe_url,
        payload: serde_json::to_vec(&json!({
            "name": thread_name,
            "auto_archive_duration": 1440
        }))
        .expect("fixed JSON thread payload serializes"),
        thread_id: message_id.to_owned(),
        parent_channel_id: channel_id.to_owned(),
    })
}

fn execute_url(
    webhook_url: &str,
    thread_id: Option<&str>,
    test_origin: Option<&str>,
) -> Result<String, TerminalReason> {
    let mut url = parse_allowed_webhook(webhook_url, test_origin)?;
    let mut query = url.query_pairs_mut();
    query.clear();
    if let Some(thread_id) = thread_id {
        validate_id(thread_id)?;
        query.append_pair("thread_id", thread_id);
    }
    query.append_pair("wait", "true");
    drop(query);
    Ok(url.into())
}

fn webhook_message_url(
    webhook_url: &str,
    thread_id: &str,
    message_id: &str,
    test_origin: Option<&str>,
) -> Result<String, TerminalReason> {
    validate_id(thread_id)?;
    validate_id(message_id)?;
    let mut url = parse_allowed_webhook(webhook_url, test_origin)?;
    let path = url.path().trim_end_matches('/');
    url.set_path(&format!("{path}/messages/{message_id}"));
    let mut query = url.query_pairs_mut();
    query.clear();
    query.append_pair("thread_id", thread_id);
    drop(query);
    Ok(url.into())
}

fn parse_allowed_webhook(value: &str, test_origin: Option<&str>) -> Result<Url, TerminalReason> {
    let url = Url::parse(value).map_err(|_| TerminalReason::AllowlistRejected)?;
    if url.fragment().is_some() || !url.username().is_empty() || url.password().is_some() {
        return Err(TerminalReason::AllowlistRejected);
    }
    if is_discord_webhook_url(value) {
        return Ok(url);
    }
    // Kept private to this module's unit tests: production callers cannot opt a local endpoint
    // into the allowlist. It lets tests exercise actual request shapes and redirect policy.
    if test_origin.is_some_and(|origin| url.as_str().starts_with(origin)) {
        return Ok(url);
    }
    Err(TerminalReason::AllowlistRejected)
}

fn message_payload(content: &str, thread_name: Option<&str>) -> Vec<u8> {
    let mut payload = json!({
        "content": content,
        "allowed_mentions": { "parse": [], "users": [], "roles": [], "replied_user": false },
    });
    if let Some(thread_name) = thread_name {
        payload["thread_name"] = Value::String(thread_name.to_owned());
    }
    serde_json::to_vec(&payload).expect("fixed JSON discussion payload serializes")
}

fn marker_payload(marker: &str) -> Vec<u8> {
    message_payload(&format!("[Artifact MCP] {marker}"), None)
}

fn validate_content(content: &str) -> Result<(), TerminalReason> {
    (!content.is_empty()
        && content.chars().count() <= MAX_DISCUSSION_CONTENT_CHARS
        && !content.contains('\0'))
    .then_some(())
    .ok_or(TerminalReason::BadRequest)
}

fn validate_thread_name(name: &str) -> Result<(), TerminalReason> {
    (!name.is_empty() && name.chars().count() <= MAX_THREAD_NAME_CHARS && !name.contains('\0'))
        .then_some(())
        .ok_or(TerminalReason::BadRequest)
}

fn validate_id(value: &str) -> Result<(), TerminalReason> {
    valid_snowflake(value)
        .then_some(())
        .ok_or(TerminalReason::BadRequest)
}

fn valid_snowflake(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_DISCUSSION_ID_BYTES
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
        && value.len() <= 128
        && suffix
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
}

fn response_headers(response: &reqwest::Response) -> BTreeMap<String, String> {
    response
        .headers()
        .iter()
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|value| (name.to_string(), value.to_owned()))
        })
        .collect()
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

fn response_thread_id(body: &[u8]) -> Option<String> {
    (body.len() <= MAX_RESPONSE_BODY_BYTES)
        .then(|| serde_json::from_slice::<Value>(body).ok())
        .flatten()
        .and_then(|value| {
            value
                .get("channel_id")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
        })
        .filter(|id| valid_snowflake(id))
}

fn parse_webhook_destination(body: &[u8]) -> Option<DiscordWebhookDestination> {
    (body.len() <= MAX_RESPONSE_BODY_BYTES)
        .then(|| serde_json::from_slice::<Value>(body).ok())
        .flatten()
        .and_then(|value| {
            let webhook_id = value.get("id")?.as_str()?.to_owned();
            let channel_id = value.get("channel_id")?.as_str()?.to_owned();
            let guild_id = value.get("guild_id")?.as_str()?.to_owned();
            (valid_snowflake(&webhook_id)
                && valid_snowflake(&channel_id)
                && valid_snowflake(&guild_id))
            .then_some(DiscordWebhookDestination {
                webhook_id,
                channel_id,
                guild_id,
            })
        })
}

fn valid_parent_channel(body: &[u8], channel_id: &str) -> bool {
    body.len() <= MAX_RESPONSE_BODY_BYTES
        && serde_json::from_slice::<Value>(body)
            .ok()
            .is_some_and(|value| {
                value.get("id").and_then(Value::as_str) == Some(channel_id)
                    && value
                        .get("type")
                        .and_then(Value::as_u64)
                        .is_some_and(|kind| matches!(kind, 0 | 5))
            })
}

fn valid_thread_channel(body: &[u8], thread_id: &str, parent_channel_id: &str) -> bool {
    if body.len() > MAX_RESPONSE_BODY_BYTES {
        return false;
    }
    serde_json::from_slice::<Value>(body)
        .ok()
        .is_some_and(|value| {
            value.get("id").and_then(Value::as_str) == Some(thread_id)
                && value.get("parent_id").and_then(Value::as_str) == Some(parent_channel_id)
                && value
                    .get("type")
                    .and_then(Value::as_u64)
                    .is_some_and(|kind| matches!(kind, 10 | 11))
        })
}

fn classify_bot_http(
    status: u16,
    headers: BTreeMap<String, String>,
    body: Vec<u8>,
    webhook_ref: &str,
) -> DiscussionResult {
    result_from_delivery(classify_http_response(&DiscordHttpResponse {
        status,
        webhook_ref: Some(webhook_ref.to_owned()),
        headers,
        body,
    }))
}

fn result_from_delivery(
    result: crate::integrations::discord_delivery::DeliveryClassification,
) -> DiscussionResult {
    match result {
        crate::integrations::discord_delivery::DeliveryClassification::Accepted {
            message_id,
            rate_limit,
        } => DiscussionResult::Accepted {
            receipt: DiscussionReceipt {
                message_id,
                thread_id: None,
            },
            rate_limit,
        },
        crate::integrations::discord_delivery::DeliveryClassification::Retry {
            reason,
            duplicate_risk,
            rate_limit,
        } => DiscussionResult::Retry {
            reason,
            duplicate_risk,
            rate_limit,
        },
        crate::integrations::discord_delivery::DeliveryClassification::Terminal { reason } => {
            DiscussionResult::Terminal { reason }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{net::SocketAddr, sync::Arc};

    use tokio::{
        io::{AsyncReadExt as _, AsyncWriteExt as _},
        net::TcpListener,
        sync::Mutex,
    };

    use super::*;

    #[derive(Clone)]
    struct FakeDiscord {
        origin: String,
        request: Arc<Mutex<Option<String>>>,
    }

    async fn fake_discord(response: &'static str, delay: Duration) -> FakeDiscord {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.expect("bind");
        let address: SocketAddr = listener.local_addr().expect("address");
        let request = Arc::new(Mutex::new(None));
        let recorded = Arc::clone(&request);
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let mut buffer = Vec::new();
            let mut scratch = [0_u8; 4096];
            loop {
                let read = socket.read(&mut scratch).await.expect("read request");
                if read == 0 {
                    break;
                }
                buffer.extend_from_slice(&scratch[..read]);
                let header_end = buffer.windows(4).position(|window| window == b"\r\n\r\n");
                let content_length = String::from_utf8_lossy(&buffer)
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length: "))
                    .and_then(|value| value.parse::<usize>().ok())
                    .unwrap_or(0);
                if header_end.is_some_and(|end| buffer.len() >= end + 4 + content_length) {
                    break;
                }
            }
            *recorded.lock().await = Some(String::from_utf8_lossy(&buffer).into_owned());
            tokio::time::sleep(delay).await;
            let _ = socket.write_all(response.as_bytes()).await;
        });
        FakeDiscord {
            origin: format!("http://{address}"),
            request,
        }
    }

    async fn fake_discord_sequence(responses: Vec<String>) -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.expect("bind");
        let address: SocketAddr = listener.local_addr().expect("address");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&requests);
        tokio::spawn(async move {
            for response in responses {
                let (mut socket, _) = listener.accept().await.expect("accept");
                let mut buffer = Vec::new();
                let mut scratch = [0_u8; 4096];
                loop {
                    let read = socket.read(&mut scratch).await.expect("read request");
                    if read == 0 {
                        break;
                    }
                    buffer.extend_from_slice(&scratch[..read]);
                    let header_end = buffer.windows(4).position(|window| window == b"\r\n\r\n");
                    let content_length = String::from_utf8_lossy(&buffer)
                        .lines()
                        .find_map(|line| line.strip_prefix("content-length: "))
                        .and_then(|value| value.parse::<usize>().ok())
                        .unwrap_or(0);
                    if header_end.is_some_and(|end| buffer.len() >= end + 4 + content_length) {
                        break;
                    }
                }
                recorded
                    .lock()
                    .await
                    .push(String::from_utf8_lossy(&buffer).into_owned());
                socket
                    .write_all(response.as_bytes())
                    .await
                    .expect("write response");
            }
        });
        (format!("http://{address}"), requests)
    }

    fn json_response(status: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    fn local_request(
        origin: &str,
        operation: DiscussionOperation,
    ) -> Result<DiscussionRequest, TerminalReason> {
        local_request_with_ref(origin, "webhook:discussion-ref", operation)
    }

    fn local_request_with_ref(
        origin: &str,
        webhook_ref: &str,
        operation: DiscussionOperation,
    ) -> Result<DiscussionRequest, TerminalReason> {
        discussion_request_inner(
            &format!("{origin}/api/webhooks/123/ULTRA-SECRET?junk=true"),
            webhook_ref.to_owned(),
            operation,
            Some(origin),
        )
    }

    #[tokio::test]
    async fn create_thread_uses_wait_name_and_disabled_mentions_and_returns_ids() {
        let fake = fake_discord(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 61\r\nConnection: close\r\n\r\n{\"id\":\"123456789012345678\",\"channel_id\":\"234567890123456789\"}",
            Duration::ZERO,
        )
        .await;
        let request = local_request(
            &fake.origin,
            DiscussionOperation::create_thread(
                "Artifact title".to_owned(),
                "@everyone hello".to_owned(),
            )
            .expect("operation"),
        )
        .expect("request");
        let result = DiscordDiscussionTransport::with_timeout(Duration::from_secs(1))
            .expect("transport")
            .deliver(request)
            .await;
        assert!(matches!(
            result,
            DiscussionResult::Accepted {
                receipt: DiscussionReceipt { message_id, thread_id: Some(thread_id) },
                ..
            } if message_id == "123456789012345678" && thread_id == "234567890123456789"
        ));
        let raw = fake.request.lock().await.clone().expect("request recorded");
        assert!(raw.starts_with("POST /api/webhooks/123/ULTRA-SECRET?wait=true HTTP/1.1"));
        assert!(!raw.contains("junk=true"));
        let body = raw.split("\r\n\r\n").nth(1).expect("body");
        let body: Value = serde_json::from_str(body).expect("json");
        assert_eq!(body["thread_name"], "Artifact title");
        assert_eq!(body["content"], "@everyone hello");
        assert_eq!(
            body["allowed_mentions"],
            json!({"parse": [], "users": [], "roles": [], "replied_user": false})
        );
    }

    #[tokio::test]
    async fn notification_thread_retry_probes_existing_thread_then_posts_comment() {
        let channel_id = "123456789012345678";
        let source_message_id = "223456789012345678";
        let comment_message_id = "323456789012345678";
        let thread_body = format!(
            "{{\"id\":\"{source_message_id}\",\"parent_id\":\"{channel_id}\",\"type\":11}}"
        );
        let comment_body = format!("{{\"id\":\"{comment_message_id}\"}}");
        let (origin, requests) = fake_discord_sequence(vec![
            json_response("400 Bad Request", "{}"),
            json_response("200 OK", &thread_body),
            json_response("200 OK", &comment_body),
        ])
        .await;
        let operation = DiscussionOperation::create_thread_from_message(
            channel_id.to_owned(),
            source_message_id.to_owned(),
            "Artifact discussion".to_owned(),
            "First comment".to_owned(),
        )
        .expect("operation");
        let request = discussion_request_inner(
            &format!("{origin}/api/webhooks/123/ULTRA-SECRET"),
            "discussion:connection-a".to_owned(),
            operation,
            Some(&origin),
        )
        .expect("request");
        let debug = format!("{request:?}");
        assert!(!debug.contains("ULTRA-SECRET"));
        assert!(!debug.contains("First comment"));
        let result = DiscordDiscussionTransport::with_timeout_and_bot_token(
            Duration::from_secs(1),
            Some(Secret::new("navi-test-token")),
        )
        .expect("transport")
        .deliver(request)
        .await;
        assert!(matches!(
            result,
            DiscussionResult::Accepted {
                receipt: DiscussionReceipt {
                    message_id,
                    thread_id: Some(thread_id),
                },
                ..
            } if message_id == comment_message_id && thread_id == source_message_id
        ));
        let requests = requests.lock().await;
        assert_eq!(requests.len(), 3);
        assert!(requests[0].starts_with(&format!(
            "POST /api/v10/channels/{channel_id}/messages/{source_message_id}/threads HTTP/1.1"
        )));
        assert!(requests[0].contains("authorization: Bot navi-test-token"));
        assert!(requests[1].starts_with(&format!(
            "GET /api/v10/channels/{source_message_id} HTTP/1.1"
        )));
        assert!(requests[2].starts_with(&format!(
            "POST /api/webhooks/123/ULTRA-SECRET?thread_id={source_message_id}&wait=true HTTP/1.1"
        )));
        assert!(!requests[2].contains("authorization: Bot "));
    }

    #[tokio::test]
    async fn notification_thread_creates_then_posts_the_first_comment_with_webhook_authorship() {
        let channel_id = "123456789012345678";
        let source_message_id = "223456789012345678";
        let comment_message_id = "323456789012345678";
        let thread_body = format!(
            "{{\"id\":\"{source_message_id}\",\"parent_id\":\"{channel_id}\",\"type\":11}}"
        );
        let comment_body = format!("{{\"id\":\"{comment_message_id}\"}}");
        let (origin, requests) = fake_discord_sequence(vec![
            json_response("201 Created", &thread_body),
            json_response("200 OK", &comment_body),
        ])
        .await;
        let request = discussion_request_inner(
            &format!("{origin}/api/webhooks/123/ULTRA-SECRET"),
            "discussion:connection-a".to_owned(),
            DiscussionOperation::create_thread_from_message(
                channel_id.to_owned(),
                source_message_id.to_owned(),
                "Artifact discussion".to_owned(),
                "First comment".to_owned(),
            )
            .expect("operation"),
            Some(&origin),
        )
        .expect("request");
        let result = DiscordDiscussionTransport::with_timeout_and_bot_token(
            Duration::from_secs(1),
            Some(Secret::new("navi-test-token")),
        )
        .expect("transport")
        .deliver(request)
        .await;
        assert!(matches!(
            result,
            DiscussionResult::Accepted {
                receipt: DiscussionReceipt {
                    message_id,
                    thread_id: Some(thread_id),
                },
                ..
            } if message_id == comment_message_id && thread_id == source_message_id
        ));
        let requests = requests.lock().await;
        assert_eq!(requests.len(), 2);
        assert!(requests[0].starts_with(&format!(
            "POST /api/v10/channels/{channel_id}/messages/{source_message_id}/threads HTTP/1.1"
        )));
        assert!(requests[0].contains("authorization: Bot navi-test-token"));
        let thread_payload: Value =
            serde_json::from_str(requests[0].split("\r\n\r\n").nth(1).expect("thread body"))
                .expect("thread JSON");
        assert_eq!(thread_payload["name"], "Artifact discussion");
        assert_eq!(thread_payload["auto_archive_duration"], 1440);
        assert!(requests[1].starts_with(&format!(
            "POST /api/webhooks/123/ULTRA-SECRET?thread_id={source_message_id}&wait=true HTTP/1.1"
        )));
        assert!(!requests[1].contains("authorization: Bot "));
    }

    #[tokio::test]
    async fn notification_thread_requires_bot_before_any_provider_call() {
        let fake = fake_discord("", Duration::ZERO).await;
        let operation = DiscussionOperation::create_thread_from_message(
            "123456789012345678".to_owned(),
            "223456789012345678".to_owned(),
            "Artifact discussion".to_owned(),
            "First comment".to_owned(),
        )
        .expect("operation");
        let result = DiscordDiscussionTransport::with_timeout(Duration::from_millis(100))
            .expect("transport")
            .deliver(
                discussion_request_inner(
                    &format!("{}/api/webhooks/123/ULTRA-SECRET", fake.origin),
                    "discussion:connection-a".to_owned(),
                    operation,
                    Some(&fake.origin),
                )
                .expect("request"),
            )
            .await;
        assert_eq!(
            result,
            DiscussionResult::Terminal {
                reason: TerminalReason::InvalidSecret
            }
        );
        assert!(fake.request.lock().await.is_none());
    }

    #[tokio::test]
    async fn discussion_connection_reference_reaches_transport_without_becoming_a_url() {
        let fake = fake_discord(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 27\r\nConnection: close\r\n\r\n{\"id\":\"123456789012345678\"}",
            Duration::ZERO,
        )
        .await;
        let request = local_request_with_ref(
            &fake.origin,
            "discussion:connection-a",
            DiscussionOperation::reply("234567890123456789".to_owned(), "reply".to_owned())
                .expect("operation"),
        )
        .expect("discussion ref request");
        let result = DiscordDiscussionTransport::with_timeout(Duration::from_secs(1))
            .expect("transport")
            .deliver(request)
            .await;
        assert!(
            matches!(result, DiscussionResult::Accepted { .. }),
            "{result:?}"
        );
        let raw = fake.request.lock().await.clone().expect("request recorded");
        assert!(!raw.contains("discussion:connection-a"));
    }

    #[tokio::test]
    async fn root_success_without_a_bounded_thread_id_is_ambiguous() {
        let fake = fake_discord(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 44\r\nConnection: close\r\n\r\n{\"id\":\"123456789012345678\",\"channel_id\":\"0\"}",
            Duration::ZERO,
        )
        .await;
        let result = DiscordDiscussionTransport::with_timeout(Duration::from_secs(1))
            .expect("transport")
            .deliver(
                local_request(
                    &fake.origin,
                    DiscussionOperation::create_thread("Artifact".to_owned(), "comment".to_owned())
                        .expect("operation"),
                )
                .expect("request"),
            )
            .await;
        assert!(matches!(
            result,
            DiscussionResult::Retry {
                reason: RetryReason::Ambiguous,
                duplicate_risk: DuplicateRisk::Possible,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn replies_markers_and_tombstones_use_the_expected_discord_shapes() {
        let cases = [
            (
                DiscussionOperation::reply("234567890123456789".to_owned(), "reply".to_owned())
                    .expect("reply"),
                "POST /api/webhooks/123/ULTRA-SECRET?thread_id=234567890123456789&wait=true HTTP/1.1",
                "reply",
            ),
            (
                DiscussionOperation::resolved_marker("234567890123456789".to_owned())
                    .expect("marker"),
                "POST /api/webhooks/123/ULTRA-SECRET?thread_id=234567890123456789&wait=true HTTP/1.1",
                "[Artifact MCP] Discussion resolved.",
            ),
            (
                DiscussionOperation::reopened_marker("234567890123456789".to_owned())
                    .expect("marker"),
                "POST /api/webhooks/123/ULTRA-SECRET?thread_id=234567890123456789&wait=true HTTP/1.1",
                "[Artifact MCP] Discussion reopened.",
            ),
            (
                DiscussionOperation::tombstone(
                    "234567890123456789".to_owned(),
                    "345678901234567890".to_owned(),
                )
                .expect("tombstone"),
                "PATCH /api/webhooks/123/ULTRA-SECRET/messages/345678901234567890?thread_id=234567890123456789 HTTP/1.1",
                "[Artifact MCP] This Artifact MCP comment was deleted.",
            ),
        ];
        for (operation, expected_start, expected_content) in cases {
            let fake = fake_discord(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 27\r\nConnection: close\r\n\r\n{\"id\":\"123456789012345678\"}",
                Duration::ZERO,
            )
            .await;
            let result = DiscordDiscussionTransport::with_timeout(Duration::from_secs(1))
                .expect("transport")
                .deliver(local_request(&fake.origin, operation).expect("request"))
                .await;
            assert!(matches!(result, DiscussionResult::Accepted { .. }));
            let raw = fake.request.lock().await.clone().expect("recorded");
            assert!(raw.starts_with(expected_start), "unexpected request: {raw}");
            let body: Value =
                serde_json::from_str(raw.split("\r\n\r\n").nth(1).expect("body")).expect("json");
            assert_eq!(body["content"], expected_content);
            assert_eq!(body["allowed_mentions"]["parse"], json!([]));
        }
    }

    #[tokio::test]
    async fn local_fake_server_covers_retry_terminal_ambiguous_and_redirect_outcomes() {
        let cases = [
            (
                "HTTP/1.1 429 Too Many Requests\r\nContent-Type: application/json\r\nContent-Length: 19\r\nConnection: close\r\n\r\n{\"retry_after\":0.1}",
                "rate_limited",
            ),
            (
                "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                "server_error",
            ),
            (
                "HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                "bad_request",
            ),
            (
                "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1/elsewhere\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                "redirect",
            ),
            (
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
                "ambiguous",
            ),
        ];
        for (response, expected) in cases {
            let fake = fake_discord(response, Duration::ZERO).await;
            let result = DiscordDiscussionTransport::with_timeout(Duration::from_secs(1))
                .expect("transport")
                .deliver(
                    local_request(
                        &fake.origin,
                        DiscussionOperation::reply(
                            "234567890123456789".to_owned(),
                            "reply".to_owned(),
                        )
                        .expect("operation"),
                    )
                    .expect("request"),
                )
                .await;
            match (expected, result) {
                (
                    "rate_limited",
                    DiscussionResult::Retry {
                        reason: RetryReason::RateLimited,
                        ..
                    },
                )
                | (
                    "server_error",
                    DiscussionResult::Retry {
                        reason: RetryReason::ServerError,
                        ..
                    },
                )
                | (
                    "bad_request",
                    DiscussionResult::Terminal {
                        reason: TerminalReason::BadRequest,
                    },
                )
                | (
                    "redirect",
                    DiscussionResult::Terminal {
                        reason: TerminalReason::Redirect,
                    },
                )
                | (
                    "ambiguous",
                    DiscussionResult::Retry {
                        reason: RetryReason::Ambiguous,
                        ..
                    },
                ) => {}
                (_, unexpected) => panic!("unexpected discussion result: {unexpected:?}"),
            }
        }
    }

    #[tokio::test]
    async fn timeout_and_network_are_retryable_and_debug_is_secret_free() {
        let fake = fake_discord("", Duration::from_secs(1)).await;
        let request = local_request(
            &fake.origin,
            DiscussionOperation::reply(
                "234567890123456789".to_owned(),
                "never-log-comment".to_owned(),
            )
            .expect("operation"),
        )
        .expect("request");
        let debug = format!("{request:?}");
        assert!(!debug.contains("ULTRA-SECRET"));
        assert!(!debug.contains("never-log-comment"));
        let timeout = DiscordDiscussionTransport::with_timeout(Duration::from_millis(20))
            .expect("transport")
            .deliver(request)
            .await;
        assert!(matches!(
            timeout,
            DiscussionResult::Retry {
                reason: RetryReason::Timeout,
                ..
            }
        ));

        let network = DiscordDiscussionTransport::with_timeout(Duration::from_millis(100))
            .expect("transport")
            .deliver(
                discussion_request_inner(
                    "http://127.0.0.1:9/api/webhooks/123/ULTRA-SECRET",
                    "webhook:discussion-ref".to_owned(),
                    DiscussionOperation::reply("234567890123456789".to_owned(), "reply".to_owned())
                        .expect("operation"),
                    Some("http://127.0.0.1:9"),
                )
                .expect("request"),
            )
            .await;
        assert!(matches!(
            network,
            DiscussionResult::Retry {
                reason: RetryReason::Network,
                ..
            }
        ));
    }

    #[test]
    fn bounds_and_production_allowlist_are_enforced_without_leaking_inputs() {
        assert!(valid_webhook_ref("webhook:legacy-ref"));
        assert!(valid_webhook_ref("discussion:connection-a"));
        assert!(!valid_webhook_ref(
            "https://discord.com/api/webhooks/123/token"
        ));
        assert!(!valid_webhook_ref("discussion:"));
        assert!(!valid_webhook_ref("other:connection-a"));
        assert_eq!(
            DiscussionOperation::create_thread(String::new(), "body".to_owned()),
            Err(TerminalReason::BadRequest)
        );
        assert_eq!(
            DiscussionOperation::reply("00".to_owned(), "body".to_owned()),
            Err(TerminalReason::BadRequest)
        );
        assert_eq!(
            discussion_request(
                "http://127.0.0.1:9/api/webhooks/123/ULTRA-SECRET",
                "webhook:discussion-ref".to_owned(),
                DiscussionOperation::resolved_marker("234567890123456789".to_owned())
                    .expect("operation"),
            ),
            Err(TerminalReason::AllowlistRejected)
        );
        assert_eq!(
            discussion_request(
                "https://discord.com/api/webhooks/123/token",
                "webhook:discussion-ref".to_owned(),
                DiscussionOperation::CreateThread {
                    thread_name: "Artifact".to_owned(),
                    content: "x".repeat(MAX_DISCUSSION_CONTENT_CHARS + 1),
                },
            ),
            Err(TerminalReason::BadRequest)
        );
        assert_eq!(
            discussion_request(
                "https://discord.com/api/webhooks/123/token",
                "webhook:discussion-ref".to_owned(),
                DiscussionOperation::Reply {
                    thread_id: "234567890123456789".to_owned(),
                    content: "contains\0nul".to_owned(),
                },
            ),
            Err(TerminalReason::BadRequest)
        );
    }
}
