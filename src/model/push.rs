//! Scheduled reminder and Web Push validation contracts.

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};

use crate::{
    error::AppError,
    model::{json::OrderedJson, state},
};

pub use crate::model::state::StateScope as ReminderScope;

/// The earliest permitted fire time, relative to the server clock.
pub const MIN_LEAD_MS: i64 = 60 * 1000;
/// The latest permitted fire time, relative to the server clock.
pub const MAX_LEAD_MS: i64 = 30 * 24 * 60 * 60 * 1000;
/// Title limit in Unicode scalar values.
pub const TITLE_MAX_CHARS: usize = 80;
/// Body limit in Unicode scalar values.
pub const BODY_MAX_CHARS: usize = 240;
/// Endpoint limit in UTF-8 bytes.
pub const ENDPOINT_MAX_BYTES: usize = 1024;
/// Device label limit in Unicode scalar values.
pub const LABEL_MAX_CHARS: usize = 60;
/// Tool error text when the feature is off.
pub const PUSH_DISABLED_MESSAGE: &str = "Web Push reminders are not configured on this server.";

const JS_MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;

/// Shared route/MCP validation and write failures, with their wire codes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PushError {
    #[error("bad_scope")]
    BadScope,
    #[error("bad_key")]
    BadKey,
    #[error("bad_time")]
    BadTime,
    #[error("bad_text")]
    BadText,
    #[error("reminder_limit")]
    ReminderLimit,
    #[error("bad_endpoint")]
    BadEndpoint,
    #[error("bad_keys")]
    BadKeys,
    #[error("push_unavailable")]
    Unavailable,
}

impl PushError {
    /// The JSON `error` code.
    #[must_use]
    pub fn code(self) -> &'static str {
        match self {
            Self::BadScope => "bad_scope",
            Self::BadKey => "bad_key",
            Self::BadTime => "bad_time",
            Self::BadText => "bad_text",
            Self::ReminderLimit => "reminder_limit",
            Self::BadEndpoint => "bad_endpoint",
            Self::BadKeys => "bad_keys",
            Self::Unavailable => "push_unavailable",
        }
    }

    /// The HTTP status for the route surface.
    #[must_use]
    pub const fn status(self) -> u16 {
        match self {
            Self::ReminderLimit => 409,
            Self::Unavailable => 500,
            _ => 400,
        }
    }
}

impl From<AppError> for PushError {
    fn from(_: AppError) -> Self {
        Self::Unavailable
    }
}

// ---------------------------------------------------------------------------
// Validation shared by routes and MCP
// ---------------------------------------------------------------------------

/// `^[A-Za-z0-9._-]{1,64}$`, identical to viewer state keys.
#[must_use]
pub fn valid_key(key: &str) -> bool {
    state::valid_key(key)
}

fn js_trim(value: &str) -> &str {
    value.trim_matches(|c: char| c == '\u{feff}' || (c.is_whitespace() && c != '\u{85}'))
}

fn forbidden_control(value: &str) -> bool {
    value.chars().any(|c| c.is_control() && c != '\n')
}

fn safe_integer(value: &OrderedJson) -> Option<i64> {
    match value {
        OrderedJson::Number(number) => number
            .as_f64()
            .filter(|v| v.fract() == 0.0 && v.abs() <= JS_MAX_SAFE_INTEGER)
            .map(|v| v as i64),
        _ => None,
    }
}

/// `reminderFireAt` — exactly one of `fire_at` (epoch ms) or `delay_seconds`, resolved to a time
/// 60 seconds to 30 days after `now_ms`.
///
/// # Errors
/// [`PushError::BadTime`] for every other input. A field that is present but `null` counts as
/// present and is not a safe integer, so it is `bad_time`.
pub fn resolve_fire_at(
    fire_at: Option<&OrderedJson>,
    delay_seconds: Option<&OrderedJson>,
    now_ms: i64,
) -> Result<i64, PushError> {
    let resolved = match (fire_at, delay_seconds) {
        (Some(fire_at), None) => safe_integer(fire_at).ok_or(PushError::BadTime)?,
        (None, Some(delay)) => {
            let delay = safe_integer(delay).ok_or(PushError::BadTime)?;
            delay
                .checked_mul(1000)
                .and_then(|delay| now_ms.checked_add(delay))
                .ok_or(PushError::BadTime)?
        }
        _ => return Err(PushError::BadTime),
    };
    let lead = resolved.saturating_sub(now_ms);
    if !(MIN_LEAD_MS..=MAX_LEAD_MS).contains(&lead) {
        return Err(PushError::BadTime);
    }
    Ok(resolved)
}

/// Validated reminder text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReminderText {
    pub title: String,
    pub body: String,
}

/// `reminderText(title, body)`: a trimmed title of 1..=80 characters and a body of 0..=240
/// characters, with no control characters other than `\n`.
///
/// # Errors
/// [`PushError::BadText`] for every other input, including a non-string or `null` body.
pub fn validate_text(
    title: Option<&OrderedJson>,
    body: Option<&OrderedJson>,
) -> Result<ReminderText, PushError> {
    let Some(OrderedJson::String(title)) = title else {
        return Err(PushError::BadText);
    };
    let body = match body {
        None => "",
        Some(OrderedJson::String(body)) => body.as_str(),
        Some(_) => return Err(PushError::BadText),
    };
    let title = js_trim(title);
    let title_chars = title.chars().count();
    if !(1..=TITLE_MAX_CHARS).contains(&title_chars)
        || body.chars().count() > BODY_MAX_CHARS
        || forbidden_control(title)
        || forbidden_control(body)
    {
        return Err(PushError::BadText);
    }
    Ok(ReminderText {
        title: title.to_owned(),
        body: body.to_owned(),
    })
}

/// Device label: at most 60 characters and no control characters.
#[must_use]
pub fn valid_label(label: &str) -> bool {
    label.chars().count() <= LABEL_MAX_CHARS && !label.chars().any(char::is_control)
}

/// `hostAllowed(hostname, allowlist)`: exact names, or `*.suffix` entries matching subdomains only.
#[must_use]
pub fn host_allowed(host: &str, allowlist: &[String]) -> bool {
    let host = host.to_ascii_lowercase();
    let unbracketed = host.trim_start_matches('[').trim_end_matches(']');
    if host.is_empty() || unbracketed.parse::<std::net::IpAddr>().is_ok() {
        return false;
    }
    allowlist.iter().any(|entry| match entry.strip_prefix('*') {
        Some(dot_suffix) if dot_suffix.starts_with('.') => {
            host.ends_with(dot_suffix) && host.len() > dot_suffix.len()
        }
        _ => host == *entry,
    })
}

/// `validPushEndpoint`: an absolute `https:` URL of at most 1024 bytes, with no userinfo, on the
/// default port, whose host is on the allowlist. The check runs on save and before every send.
#[must_use]
pub fn valid_endpoint(endpoint: &str, allowlist: &[String]) -> bool {
    if endpoint.is_empty() || endpoint.len() > ENDPOINT_MAX_BYTES {
        return false;
    }
    let Ok(url) = url::Url::parse(endpoint) else {
        return false;
    };
    url.scheme() == "https"
        && url.username().is_empty()
        && url.password().is_none()
        // The URL parser drops an explicit default port, so `:443` also yields `None`.
        && url.port().is_none()
        && matches!(url.host(), Some(url::Host::Domain(host)) if host_allowed(host, allowlist))
}

/// Decode canonical base64url with optional `=` padding.
#[must_use]
pub fn base64url_bytes(value: &str) -> Option<Vec<u8>> {
    let unpadded = value.trim_end_matches('=');
    if value.len() - unpadded.len() > 2
        || unpadded.is_empty()
        || !unpadded
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return None;
    }
    let bytes = URL_SAFE_NO_PAD.decode(unpadded).ok()?;
    (URL_SAFE_NO_PAD.encode(&bytes) == unpadded).then_some(bytes)
}

/// `validPushKeys`: `p256dh` is a valid uncompressed P-256 point, `auth` is 16 bytes.
#[must_use]
pub fn valid_keys(p256dh: &str, auth: &str) -> bool {
    let (Some(point), Some(auth)) = (base64url_bytes(p256dh), base64url_bytes(auth)) else {
        return false;
    };
    point.len() == 65
        && point[0] == 0x04
        && auth.len() == 16
        && p256::PublicKey::from_sec1_bytes(&point).is_ok()
}

/// Lowercased viewer email, the identity stored in every push table.
#[must_use]
pub fn normalize_email(value: &str) -> String {
    value.trim().to_lowercase()
}

/// `owner` column: `''` for org scope, the lowercased email for viewer scope.
#[must_use]
pub fn reminder_owner(scope: ReminderScope, viewer_email: Option<&str>) -> String {
    match scope {
        ReminderScope::Org => String::new(),
        ReminderScope::Viewer => viewer_email.map(normalize_email).unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_rules_match_exact_names_and_subdomain_wildcards() {
        let hosts = vec![
            "fcm.googleapis.com".to_owned(),
            "*.push.apple.com".to_owned(),
        ];
        assert!(host_allowed("fcm.googleapis.com", &hosts));
        assert!(host_allowed("api.push.apple.com", &hosts));
        assert!(!host_allowed("push.apple.com", &hosts));
        assert!(!host_allowed("evilpush.apple.com", &hosts));
        assert!(!host_allowed("x.fcm.googleapis.com", &hosts));
    }
}
