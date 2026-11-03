//! Web Push (ADR-0012) configuration and VAPID identity.

use base64::Engine as _;

use crate::error::AppError;

#[cfg(doc)]
use super::AppConfig;
use super::{
    env::{EnvSource, Secret},
    parse::present,
};

/// Default push-service allowlist — `DEFAULT_WEB_PUSH_ENDPOINT_HOSTS` in [lib/config.js].
pub const DEFAULT_WEB_PUSH_ENDPOINT_HOSTS: [&str; 6] = [
    "fcm.googleapis.com",
    "updates.push.services.mozilla.com",
    "push.services.mozilla.com",
    "web.push.apple.com",
    "*.push.apple.com",
    "*.notify.windows.com",
];

const WEB_PUSH_KEY_ERROR: &str = "WEB_PUSH_VAPID_PRIVATE_KEY must be the base64url (no padding) encoding of a 32-byte P-256 private key";
const WEB_PUSH_SUBJECT_ERROR: &str = "WEB_PUSH_SUBJECT must be a mailto: or https: contact URL when WEB_PUSH_VAPID_PRIVATE_KEY is set";
const WEB_PUSH_HOSTS_ERROR: &str =
    "WEB_PUSH_ENDPOINT_HOSTS must be a comma-separated list of host names or *.suffix entries";

/// VAPID identity for the Web Push sender. Present only when the operator configured a valid key
/// and subject.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VapidIdentity {
    /// Raw 32-byte P-256 scalar, base64url without padding. Never logged.
    pub private_key: Secret,
    /// Uncompressed public point (65 bytes), base64url without padding.
    pub public_key: String,
    /// `mailto:` or `https:` contact placed in the JWT `sub` claim.
    pub subject: String,
}

/// `parseWebPushConfig(env)` — [lib/config.js].
///
/// The feature is enabled only when a VAPID identity is configured **and** `WEBHOOK_ENC_KEY` is
/// set, because subscription endpoints are stored encrypted with that key. See
/// [`AppConfig::web_push_enabled`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WebPushConfig {
    /// `WEB_PUSH_VAPID_PRIVATE_KEY` + `WEB_PUSH_SUBJECT`.
    pub vapid: Option<VapidIdentity>,
    /// `WEB_PUSH_ENDPOINT_HOSTS`, lowercased and de-duplicated in declaration order.
    pub endpoint_hosts: Vec<String>,
}

impl Default for WebPushConfig {
    fn default() -> Self {
        Self {
            vapid: None,
            endpoint_hosts: DEFAULT_WEB_PUSH_ENDPOINT_HOSTS
                .iter()
                .map(|host| (*host).to_owned())
                .collect(),
        }
    }
}

impl WebPushConfig {
    pub(super) fn from_source(env: &dyn EnvSource) -> Result<Self, AppError> {
        let endpoint_hosts =
            parse_web_push_endpoint_hosts(present(env, "WEB_PUSH_ENDPOINT_HOSTS").as_deref())?;
        let Some(raw_key) = present(env, "WEB_PUSH_VAPID_PRIVATE_KEY") else {
            return Ok(Self {
                vapid: None,
                endpoint_hosts,
            });
        };
        let raw_key = raw_key.trim();
        let public_key = vapid_public_key(raw_key)
            .ok_or_else(|| AppError::Validation(WEB_PUSH_KEY_ERROR.to_owned()))?;
        let subject = env.get("WEB_PUSH_SUBJECT").unwrap_or_default();
        let subject = subject.trim();
        if !valid_web_push_subject(subject) {
            return Err(AppError::Validation(WEB_PUSH_SUBJECT_ERROR.to_owned()));
        }
        Ok(Self {
            vapid: Some(VapidIdentity {
                private_key: Secret::new(raw_key),
                public_key,
                subject: subject.to_owned(),
            }),
            endpoint_hosts,
        })
    }
}

/// `parseWebPushEndpointHosts(value)` — [lib/config.js].
fn parse_web_push_endpoint_hosts(raw: Option<&str>) -> Result<Vec<String>, AppError> {
    let mut hosts: Vec<String> = Vec::new();
    for entry in raw
        .unwrap_or_default()
        .split(',')
        .map(|part| part.trim().to_lowercase())
        .filter(|part| !part.is_empty())
    {
        if !valid_host_entry(&entry) {
            return Err(AppError::Validation(WEB_PUSH_HOSTS_ERROR.to_owned()));
        }
        if !hosts.contains(&entry) {
            hosts.push(entry);
        }
    }
    if hosts.is_empty() {
        return Ok(WebPushConfig::default().endpoint_hosts);
    }
    Ok(hosts)
}

/// A host name of at least two labels (also after `*.`), with no IP literal and no all-numeric
/// label. The character set already excludes `:` and brackets, so IPv6 literals never pass.
fn valid_host_entry(entry: &str) -> bool {
    let name = entry.strip_prefix("*.").unwrap_or(entry);
    (1..=253).contains(&name.len())
        && name.parse::<std::net::IpAddr>().is_err()
        && name.split('.').count() >= 2
        && name.split('.').all(|label| {
            !label.bytes().all(|b| b.is_ascii_digit())
                && (1..=63).contains(&label.len())
                && label
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
                && !label.starts_with('-')
                && !label.ends_with('-')
        })
}

/// `parseVapidPrivateKey(value).publicKey` — [lib/config.js]. `None` for any value that is not
/// the canonical unpadded base64url encoding of a valid P-256 scalar.
#[must_use]
pub fn vapid_public_key(encoded: &str) -> Option<String> {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    if encoded.len() != 43
        || !encoded
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return None;
    }
    let scalar = URL_SAFE_NO_PAD.decode(encoded).ok()?;
    if scalar.len() != 32 || URL_SAFE_NO_PAD.encode(&scalar) != encoded {
        return None;
    }
    let secret = p256::SecretKey::from_slice(&scalar).ok()?;
    let point = p256::elliptic_curve::sec1::ToSec1Point::to_sec1_point(&secret.public_key(), false);
    Some(URL_SAFE_NO_PAD.encode(point.as_bytes()))
}

/// `validWebPushSubject(value)` — [lib/config.js].
#[must_use]
pub fn valid_web_push_subject(subject: &str) -> bool {
    if subject.is_empty()
        || subject.chars().count() > 256
        || subject.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return false;
    }
    if let Some(rest) = subject.strip_prefix("mailto:") {
        return !rest.is_empty();
    }
    if !subject.starts_with("https:") {
        return false;
    }
    url::Url::parse(subject).is_ok_and(|url| {
        url.scheme() == "https"
            && url.host_str().is_some_and(|host| !host.is_empty())
            && url.username().is_empty()
            && url.password().is_none()
    })
}
