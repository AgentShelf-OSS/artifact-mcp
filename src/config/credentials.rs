//! Validation of secret-bearing environment values.

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};

use crate::error::AppError;

use super::env::Secret;

/// Port of `parseEncryptionKey(value)` — [lib/crypto.js:9-18].
///
/// The canonical re-encode check rejects non-canonical base64 exactly as Node does.
pub(super) fn parse_webhook_enc_key(value: Option<&str>) -> Result<Option<Secret>, AppError> {
    let Some(raw) = value else {
        return Ok(None);
    };
    let encoded = raw.trim();
    let decoded = BASE64_STANDARD
        .decode(encoded)
        .map_err(|_| webhook_key_error())?;
    if decoded.len() != 32 || BASE64_STANDARD.encode(&decoded) != encoded {
        return Err(webhook_key_error());
    }
    Ok(Some(Secret::new(encoded)))
}

fn webhook_key_error() -> AppError {
    AppError::Validation("WEBHOOK_ENC_KEY must be a 32-byte base64 value.".to_owned())
}

pub(super) fn parse_discord_bot_token(value: Option<&str>) -> Result<Option<Secret>, AppError> {
    let Some(raw) = value else {
        return Ok(None);
    };
    let token = raw.trim();
    if token.is_empty()
        || token.len() > 512
        || !token
            .bytes()
            .all(|byte| byte.is_ascii_graphic() && !matches!(byte, b'"' | b'\\'))
    {
        return Err(AppError::Validation(
            "DISCORD_BOT_TOKEN must be a non-empty printable token up to 512 bytes.".to_owned(),
        ));
    }
    Ok(Some(Secret::new(token)))
}

pub(super) fn parse_audit_ledger_hmac_key(value: Option<&str>) -> Result<Option<Secret>, AppError> {
    let Some(raw) = value else {
        return Ok(None);
    };
    let encoded = raw.trim();
    let decoded = BASE64_STANDARD
        .decode(encoded)
        .map_err(|_| audit_ledger_key_error())?;
    if decoded.len() != 32 || BASE64_STANDARD.encode(&decoded) != encoded {
        return Err(audit_ledger_key_error());
    }
    Ok(Some(Secret::new(encoded)))
}

fn audit_ledger_key_error() -> AppError {
    AppError::Validation(
        "AUDIT_LEDGER_HMAC_KEY must be canonical base64 encoding of exactly 32 bytes.".to_owned(),
    )
}
