//! Exact, bounded historical Discord notification recovery.
//!
//! The adapter retains only message IDs, provider webhook IDs, and embed URLs long enough to
//! compare them. Message content, titles, provider errors, and response bodies never cross this
//! module or enter persistence.

use std::{collections::BTreeSet, time::Duration};

use reqwest::header::{AUTHORIZATION, USER_AGENT};
use serde_json::Value;

use crate::{config::Secret, error::AppError};

const MAX_PAGES: usize = 10;
const PAGE_SIZE: usize = 100;
const MAX_MESSAGES: usize = 500;
const MAX_RESPONSE_BYTES: usize = 1_048_576;
const RECOVERY_DEADLINE: Duration = Duration::from_secs(10);
const USER_AGENT_VALUE: &str = "Artifact-MCP (discord-history-recovery)";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HistoryDestination {
    pub guild_id: String,
    pub channel_id: String,
    pub provider_webhook_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HistoryArtifact {
    pub canonical_url: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HistoryMessage {
    pub id: String,
    pub guild_id: String,
    pub channel_id: String,
    pub webhook_id: String,
    pub embed_urls: Vec<String>,
    pub embeds_observable: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HistoryPage {
    pub messages: Vec<HistoryMessage>,
    pub next_before: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HistoryProviderFailure {
    PermissionDenied,
    RateLimited,
    Unavailable,
}

pub trait DiscordHistoryProvider: Send + Sync {
    fn list_messages<'a>(
        &'a self,
        credential: &'a Secret,
        destination: &'a HistoryDestination,
        before: Option<&'a str>,
    ) -> crate::ports::BoxFuture<'a, Result<HistoryPage, HistoryProviderFailure>>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExactRecoveryOutcome {
    Recovered { message_id: String },
    NotFound,
    Ambiguous,
    PermissionDenied,
    RateLimited,
    Retryable,
}

/// Match one and only one message from the exact configured provider webhook and canonical URL.
pub async fn recover_exact(
    provider: &dyn DiscordHistoryProvider,
    credential: &Secret,
    destination: &HistoryDestination,
    artifact: &HistoryArtifact,
) -> ExactRecoveryOutcome {
    if !valid_id(&destination.guild_id)
        || !valid_id(&destination.channel_id)
        || !valid_id(&destination.provider_webhook_id)
        || !valid_canonical_url(&artifact.canonical_url)
    {
        return ExactRecoveryOutcome::Retryable;
    }
    let deadline = tokio::time::Instant::now() + RECOVERY_DEADLINE;
    let mut before: Option<String> = None;
    let mut seen_cursors = BTreeSet::new();
    let mut matches = BTreeSet::new();
    let mut scanned = 0_usize;
    for _ in 0..MAX_PAGES {
        if scanned >= MAX_MESSAGES || tokio::time::Instant::now() >= deadline {
            return ExactRecoveryOutcome::Retryable;
        }
        if let Some(cursor) = before.as_ref()
            && !seen_cursors.insert(cursor.clone())
        {
            return ExactRecoveryOutcome::Retryable;
        }
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let page = match tokio::time::timeout(
            remaining,
            provider.list_messages(credential, destination, before.as_deref()),
        )
        .await
        {
            Err(_) => return ExactRecoveryOutcome::Retryable,
            Ok(Err(HistoryProviderFailure::PermissionDenied)) => {
                return ExactRecoveryOutcome::PermissionDenied;
            }
            Ok(Err(HistoryProviderFailure::RateLimited)) => {
                return ExactRecoveryOutcome::RateLimited;
            }
            Ok(Err(HistoryProviderFailure::Unavailable)) => {
                return ExactRecoveryOutcome::Retryable;
            }
            Ok(Ok(page)) => page,
        };
        for message in page.messages.into_iter().take(MAX_MESSAGES - scanned) {
            scanned += 1;
            if message.guild_id != destination.guild_id
                || message.channel_id != destination.channel_id
                || message.webhook_id != destination.provider_webhook_id
            {
                continue;
            }
            if !message.embeds_observable {
                return ExactRecoveryOutcome::Retryable;
            }
            if message
                .embed_urls
                .iter()
                .any(|url| url == &artifact.canonical_url)
            {
                matches.insert(message.id);
                if matches.len() > 1 {
                    return ExactRecoveryOutcome::Ambiguous;
                }
            }
        }
        match page.next_before {
            Some(cursor) if !cursor.is_empty() => before = Some(cursor),
            _ => {
                return matches
                    .into_iter()
                    .next()
                    .map_or(ExactRecoveryOutcome::NotFound, |message_id| {
                        ExactRecoveryOutcome::Recovered { message_id }
                    });
            }
        }
    }
    ExactRecoveryOutcome::Retryable
}

pub struct DiscordHistoryRest {
    client: reqwest::Client,
}

impl DiscordHistoryRest {
    pub fn new() -> Result<Self, AppError> {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(4))
            .build()
            .map_err(|_| AppError::Unavailable("Discord recovery unavailable.".to_owned()))?;
        Ok(Self { client })
    }
}

impl DiscordHistoryProvider for DiscordHistoryRest {
    fn list_messages<'a>(
        &'a self,
        credential: &'a Secret,
        destination: &'a HistoryDestination,
        before: Option<&'a str>,
    ) -> crate::ports::BoxFuture<'a, Result<HistoryPage, HistoryProviderFailure>> {
        Box::pin(async move {
            if before.is_some_and(|cursor| !valid_id(cursor)) {
                return Err(HistoryProviderFailure::Unavailable);
            }
            let mut url = format!(
                "https://discord.com/api/v10/channels/{}/messages?limit={PAGE_SIZE}",
                destination.channel_id
            );
            if let Some(before) = before {
                url.push_str("&before=");
                url.push_str(before);
            }
            let request = self
                .client
                .get(url)
                .header(AUTHORIZATION, format!("Bot {}", credential.expose()))
                .header(USER_AGENT, USER_AGENT_VALUE);
            let mut response = request
                .send()
                .await
                .map_err(|_| HistoryProviderFailure::Unavailable)?;
            match response.status().as_u16() {
                401 | 403 | 404 => return Err(HistoryProviderFailure::PermissionDenied),
                429 => return Err(HistoryProviderFailure::RateLimited),
                status if !(200..=299).contains(&status) => {
                    return Err(HistoryProviderFailure::Unavailable);
                }
                _ => {}
            }
            if response
                .content_length()
                .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
            {
                return Err(HistoryProviderFailure::Unavailable);
            }
            let mut body = Vec::new();
            while let Some(chunk) = response
                .chunk()
                .await
                .map_err(|_| HistoryProviderFailure::Unavailable)?
            {
                if body.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
                    return Err(HistoryProviderFailure::Unavailable);
                }
                body.extend_from_slice(&chunk);
            }
            let values: Vec<Value> =
                serde_json::from_slice(&body).map_err(|_| HistoryProviderFailure::Unavailable)?;
            if values.len() > PAGE_SIZE {
                return Err(HistoryProviderFailure::Unavailable);
            }
            let mut messages = Vec::with_capacity(values.len());
            for value in &values {
                let Some(id) = value
                    .get("id")
                    .and_then(Value::as_str)
                    .filter(|id| valid_id(id))
                else {
                    continue;
                };
                let webhook_id = value
                    .get("webhook_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let guild_id = value
                    .get("guild_id")
                    .and_then(Value::as_str)
                    .unwrap_or(destination.guild_id.as_str());
                let channel_id = value
                    .get("channel_id")
                    .and_then(Value::as_str)
                    .unwrap_or(destination.channel_id.as_str());
                let embeds = value.get("embeds").and_then(Value::as_array);
                // An exact selected-webhook message with no observable embed cannot prove which
                // artifact notification it belongs to. Discord may redact embed content while
                // suppressing embeds, so both a missing field and an empty array are a
                // capability failure, never evidence that the notification is absent.
                let embeds_observable = observable_embeds(value);
                let embed_urls = embeds
                    .into_iter()
                    .flatten()
                    .take(10)
                    .filter_map(|embed| embed.get("url").and_then(Value::as_str))
                    .filter(|url| url.len() <= 2_048)
                    .map(ToOwned::to_owned)
                    .collect();
                messages.push(HistoryMessage {
                    id: id.to_owned(),
                    guild_id: guild_id.to_owned(),
                    channel_id: channel_id.to_owned(),
                    webhook_id: webhook_id.to_owned(),
                    embed_urls,
                    embeds_observable,
                });
            }
            let next_before = (values.len() == PAGE_SIZE)
                .then(|| values.last()?.get("id")?.as_str().map(ToOwned::to_owned))
                .flatten();
            Ok(HistoryPage {
                messages,
                next_before,
            })
        })
    }
}

fn observable_embeds(value: &Value) -> bool {
    value
        .get("embeds")
        .and_then(Value::as_array)
        .is_some_and(|embeds| !embeds.is_empty())
}

fn valid_id(value: &str) -> bool {
    !value.is_empty() && value.len() <= 32 && value.bytes().all(|byte| byte.is_ascii_digit())
}

fn valid_canonical_url(value: &str) -> bool {
    value.len() <= 2_048
        && url::Url::parse(value).is_ok_and(|url| {
            url.scheme() == "https"
                && url.host_str().is_some()
                && url.username().is_empty()
                && url.password().is_none()
                && url.fragment().is_none()
        })
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    struct Fake {
        pages: Mutex<Vec<Result<HistoryPage, HistoryProviderFailure>>>,
    }

    impl DiscordHistoryProvider for Fake {
        fn list_messages<'a>(
            &'a self,
            _credential: &'a Secret,
            _destination: &'a HistoryDestination,
            _before: Option<&'a str>,
        ) -> crate::ports::BoxFuture<'a, Result<HistoryPage, HistoryProviderFailure>> {
            Box::pin(async move { self.pages.lock().expect("pages").remove(0) })
        }
    }

    fn destination() -> HistoryDestination {
        HistoryDestination {
            guild_id: "123456789012345678".into(),
            channel_id: "223456789012345678".into(),
            provider_webhook_id: "323456789012345678".into(),
        }
    }

    fn message(id: &str, webhook_id: &str) -> HistoryMessage {
        HistoryMessage {
            id: id.into(),
            guild_id: "123456789012345678".into(),
            channel_id: "223456789012345678".into(),
            webhook_id: webhook_id.into(),
            embed_urls: vec!["https://artifact.example.test/a".into()],
            embeds_observable: true,
        }
    }

    #[tokio::test]
    async fn exact_provider_webhook_and_url_recovers_once() {
        let fake = Fake {
            pages: Mutex::new(vec![Ok(HistoryPage {
                messages: vec![message("423456789012345678", "323456789012345678")],
                next_before: None,
            })]),
        };
        assert_eq!(
            recover_exact(
                &fake,
                &Secret::new("synthetic"),
                &destination(),
                &HistoryArtifact {
                    canonical_url: "https://artifact.example.test/a".into()
                }
            )
            .await,
            ExactRecoveryOutcome::Recovered {
                message_id: "423456789012345678".into()
            }
        );
    }

    #[tokio::test]
    async fn local_registration_id_cannot_match_provider_webhook_and_ambiguity_fails_closed() {
        let fake = Fake {
            pages: Mutex::new(vec![Ok(HistoryPage {
                messages: vec![
                    message("423456789012345678", "local-webhook-registration"),
                    message("523456789012345678", "323456789012345678"),
                    message("623456789012345678", "323456789012345678"),
                ],
                next_before: None,
            })]),
        };
        assert_eq!(
            recover_exact(
                &fake,
                &Secret::new("synthetic"),
                &destination(),
                &HistoryArtifact {
                    canonical_url: "https://artifact.example.test/a".into()
                }
            )
            .await,
            ExactRecoveryOutcome::Ambiguous
        );
    }

    #[tokio::test]
    async fn permission_rate_limit_and_redacted_embeds_are_actionable_and_body_free() {
        for (reply, expected) in [
            (
                Err(HistoryProviderFailure::PermissionDenied),
                ExactRecoveryOutcome::PermissionDenied,
            ),
            (
                Err(HistoryProviderFailure::RateLimited),
                ExactRecoveryOutcome::RateLimited,
            ),
            (
                Ok(HistoryPage {
                    messages: vec![HistoryMessage {
                        embeds_observable: false,
                        embed_urls: vec![],
                        ..message("423456789012345678", "323456789012345678")
                    }],
                    next_before: None,
                }),
                ExactRecoveryOutcome::Retryable,
            ),
        ] {
            let fake = Fake {
                pages: Mutex::new(vec![reply]),
            };
            assert_eq!(
                recover_exact(
                    &fake,
                    &Secret::new("synthetic"),
                    &destination(),
                    &HistoryArtifact {
                        canonical_url: "https://artifact.example.test/a".into()
                    }
                )
                .await,
                expected
            );
        }
    }

    #[test]
    fn empty_or_missing_provider_embeds_are_not_observable() {
        assert!(!observable_embeds(&serde_json::json!({})));
        assert!(!observable_embeds(&serde_json::json!({ "embeds": [] })));
        assert!(observable_embeds(&serde_json::json!({
            "embeds": [{ "url": "https://artifact.example.test/a" }]
        })));
    }
}
