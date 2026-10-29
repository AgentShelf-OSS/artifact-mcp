use super::*;

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

/// Decode canonical base64url (optional `=` padding), as `base64urlBytes` in `lib/push.js`.
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

/// Hex SHA-256 of the endpoint string, the subscription dedupe key.
#[must_use]
pub fn endpoint_sha256(endpoint: &str) -> String {
    hex::encode(Sha256::digest(endpoint.as_bytes()))
}

/// One-column form of the `WEBHOOK_ENC_KEY` record: `v1:<nonce>:<ciphertext>:<tag>` (standard
/// base64), shared with `packEndpointCiphertext` in `lib/push.js`.
#[must_use]
pub fn pack_endpoint_ciphertext(record: &EncryptedUrl) -> String {
    format!("v1:{}:{}:{}", record.nonce, record.ciphertext, record.tag)
}

/// Inverse of [`pack_endpoint_ciphertext`].
#[must_use]
pub fn unpack_endpoint_ciphertext(value: &str) -> Option<EncryptedUrl> {
    let mut parts = value.split(':');
    let (Some("v1"), Some(nonce), Some(ciphertext), Some(tag), None) = (
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
    ) else {
        return None;
    };
    let valid = |part: &str| BASE64_STANDARD.decode(part).is_ok();
    (valid(nonce) && valid(ciphertext) && valid(tag)).then(|| EncryptedUrl {
        ciphertext: ciphertext.to_owned(),
        nonce: nonce.to_owned(),
        tag: tag.to_owned(),
    })
}

/// Lowercased viewer email, the identity stored in every push table.
#[must_use]
pub fn normalize_email(value: &str) -> String {
    value.trim().to_lowercase()
}
