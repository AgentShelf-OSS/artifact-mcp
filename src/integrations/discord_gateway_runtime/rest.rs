use std::time::Duration;

use reqwest::{StatusCode, header::AUTHORIZATION};
use twilight_model::channel::Message;

use crate::{config::Secret, error::AppError};

const DISCORD_API: &str = "https://discord.com/api/v10";
const USER_AGENT: &str = "artifact-mcp/1.6 (+https://github.com/nb-artifact/artifact-mcp)";
const REST_TIMEOUT: Duration = Duration::from_secs(4);

pub struct DiscordInboundReadinessRest {
    client: reqwest::Client,
    api_base: String,
}

pub(super) enum MessageFetch {
    Found(Box<Message>),
    Missing,
    Retryable {
        after_seconds: u16,
        rate_limited: bool,
    },
}

impl DiscordInboundReadinessRest {
    pub fn new() -> Result<Self, AppError> {
        Self::with_api_base(DISCORD_API)
    }

    #[cfg(test)]
    pub(super) fn with_api_base(api_base: &str) -> Result<Self, AppError> {
        Self::build_with_api_base(api_base)
    }

    #[cfg(not(test))]
    fn with_api_base(api_base: &str) -> Result<Self, AppError> {
        Self::build_with_api_base(api_base)
    }

    fn build_with_api_base(api_base: &str) -> Result<Self, AppError> {
        reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(REST_TIMEOUT)
            .user_agent(USER_AGENT)
            .build()
            .map(|client| Self {
                client,
                api_base: api_base.trim_end_matches('/').to_owned(),
            })
            .map_err(|_| AppError::Unavailable("Discord inbound readiness unavailable.".to_owned()))
    }

    pub async fn validate_thread(
        &self,
        credential: &Secret,
        thread_id: &str,
    ) -> Result<(), AppError> {
        self.validate_channel(credential, thread_id).await?;
        let url = resource_url(
            &self.api_base,
            &format!("channels/{thread_id}/messages?limit=1"),
        )?;
        let response = self
            .client
            .get(url)
            .header(AUTHORIZATION, format!("Bot {}", credential.expose()))
            .send()
            .await
            .map_err(|_| {
                AppError::Unavailable("Discord inbound readiness unavailable.".to_owned())
            })?;
        classify_readiness_status(response.status())
    }

    pub(super) async fn validate_channel(
        &self,
        credential: &Secret,
        channel_id: &str,
    ) -> Result<(), AppError> {
        let url = resource_url(&self.api_base, &format!("channels/{channel_id}"))?;
        let response = self
            .client
            .get(url)
            .header(AUTHORIZATION, format!("Bot {}", credential.expose()))
            .send()
            .await
            .map_err(|_| {
                AppError::Unavailable("Discord inbound readiness unavailable.".to_owned())
            })?;
        classify_readiness_status(response.status())
    }

    pub(super) async fn fetch_message(
        &self,
        credential: &Secret,
        channel_id: &str,
        message_id: &str,
    ) -> MessageFetch {
        let Ok(url) = resource_url(
            &self.api_base,
            &format!("channels/{channel_id}/messages/{message_id}"),
        ) else {
            return MessageFetch::Missing;
        };
        let Ok(response) = self
            .client
            .get(url)
            .header(AUTHORIZATION, format!("Bot {}", credential.expose()))
            .send()
            .await
        else {
            return MessageFetch::Retryable {
                after_seconds: 5,
                rate_limited: false,
            };
        };
        if response.status() == StatusCode::NOT_FOUND {
            return MessageFetch::Missing;
        }
        if response.status() == StatusCode::TOO_MANY_REQUESTS {
            let after_seconds = response
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse::<f64>().ok())
                .filter(|value| value.is_finite() && *value > 0.0)
                .map(|value| value.ceil().clamp(1.0, 60.0) as u16)
                .unwrap_or(5);
            return MessageFetch::Retryable {
                after_seconds,
                rate_limited: true,
            };
        }
        if !response.status().is_success() {
            return MessageFetch::Retryable {
                after_seconds: 5,
                rate_limited: false,
            };
        }
        response.json::<Message>().await.map(Box::new).map_or(
            MessageFetch::Retryable {
                after_seconds: 5,
                rate_limited: false,
            },
            MessageFetch::Found,
        )
    }
}

#[cfg(test)]
pub(super) fn discord_resource_url(api_base: &str, path: &str) -> Result<String, AppError> {
    resource_url(api_base, path)
}

fn resource_url(api_base: &str, path: &str) -> Result<String, AppError> {
    let identifiers_valid = path
        .split(['/', '?', '='])
        .filter(|part| !matches!(*part, "channels" | "messages" | "limit" | "1"))
        .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()));
    if !identifiers_valid {
        return Err(AppError::Validation(
            "Discord provider identity is invalid.".to_owned(),
        ));
    }
    Ok(format!("{api_base}/{path}"))
}

fn classify_readiness_status(status: StatusCode) -> Result<(), AppError> {
    match status {
        status if status.is_success() => Ok(()),
        StatusCode::UNAUTHORIZED => Err(AppError::Validation(
            "The organization Discord credential is not valid.".to_owned(),
        )),
        StatusCode::FORBIDDEN | StatusCode::NOT_FOUND => Err(AppError::Conflict(
            "The Discord bot cannot read the selected channel or thread.".to_owned(),
        )),
        _ => Err(AppError::Unavailable(
            "Discord inbound readiness unavailable.".to_owned(),
        )),
    }
}

pub(super) fn readiness_error(error: &AppError) -> &'static str {
    match error {
        AppError::Validation(_) => "missing_credential",
        AppError::Conflict(_) => "guild_access",
        _ => "gateway_unavailable",
    }
}
