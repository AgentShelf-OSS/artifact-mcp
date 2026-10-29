use crate::{
    error::AppError,
    integrations::discord_inbound::{DiscordMessage, InboundEvent},
};

use super::common::{MAX_PROVIDER_ID_BYTES, MAX_SAFE_ERROR_BYTES, MAX_SESSION_ID_BYTES};

pub(super) fn validate_event_envelope(event: &InboundEvent) -> Result<(), AppError> {
    for (label, value, max) in [
        ("event id", event.event_id.as_str(), MAX_PROVIDER_ID_BYTES),
        (
            "gateway session id",
            event.gateway_session_id.as_str(),
            MAX_SESSION_ID_BYTES,
        ),
        ("guild id", event.guild_id.as_str(), MAX_PROVIDER_ID_BYTES),
        ("thread id", event.thread_id.as_str(), MAX_PROVIDER_ID_BYTES),
    ] {
        if value.is_empty() || value.len() > max {
            return Err(AppError::Validation(format!("Discord {label} is invalid.")));
        }
    }
    if event.payload_fingerprint.len() != 64
        || !event
            .payload_fingerprint
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(AppError::Validation(
            "Discord event fingerprint is invalid.".to_owned(),
        ));
    }
    if let Some(message) = event.message.as_ref() {
        validate_message(message)?;
    }
    debug_assert!(MAX_SAFE_ERROR_BYTES >= "thread_unavailable".len());
    Ok(())
}

fn validate_message(message: &DiscordMessage) -> Result<(), AppError> {
    for value in [
        message.id.as_str(),
        message.guild_id.as_str(),
        message.thread_id.as_str(),
        message.author.id.as_str(),
    ] {
        if value.is_empty() || value.len() > MAX_PROVIDER_ID_BYTES {
            return Err(AppError::Validation(
                "Discord message identity is invalid.".to_owned(),
            ));
        }
    }
    if message.author.display.trim().is_empty() || message.author.display.len() > 160 {
        return Err(AppError::Validation(
            "Discord author display is invalid.".to_owned(),
        ));
    }
    Ok(())
}
