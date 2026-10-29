use std::{collections::BTreeMap, fmt, time::Duration};

use serde_json::Value;

use crate::{
    config::Secret,
    error::AppError,
    integrations::discord_delivery::{
        DISCORD_DELIVERY_TIMEOUT, DiscordHttpResponse, ProviderFault, TerminalReason,
        classify_fault, classify_http_response,
    },
};

use super::request::{
    DiscordWebhookDestination, DiscussionOperation, DiscussionReceipt, DiscussionRequest,
    DiscussionResult, MAX_RESPONSE_BODY_BYTES, MAX_THREAD_NAME_CHARS, ThreadAnchorRequest,
    discussion_request, execute_url, message_payload, parse_allowed_webhook, valid_snowflake,
    valid_webhook_ref, validate_id,
};

const DISCORD_USER_AGENT: &str = "Artifact-MCP (discord-thread-bridge)";

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

    pub(super) fn with_timeout_and_bot_token(
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
