//! Cloudflare Access identity configuration and Access policy sync.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
};

use crate::error::AppError;

use super::{
    defaults::{DEFAULT_ACCESS_CLOCK_TOLERANCE_SECONDS, DEFAULT_PUBLIC_BASE_URL},
    env::{EnvSource, Secret},
    parse::{disabled_unless_one, flag, invalid, non_empty_string, present, raw_string},
};

/// `ACCESS_IDENTITY_MODE` — [lib/identity.js:50-55]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AccessIdentityMode {
    /// Both `CF_ACCESS_TEAM_DOMAIN` and `CF_ACCESS_AUD` are set: JWT-verified identity.
    Jwt,
    /// `TRUST_ACCESS_HEADERS=1` without JWT verification: unverified header trust.
    HeaderTrust,
    /// Neither configured: identity resolution fails closed.
    #[default]
    Disabled,
}

impl AccessIdentityMode {
    /// The exact label the Node startup banner uses.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Jwt => "jwt",
            Self::HeaderTrust => "header-trust",
            Self::Disabled => "disabled",
        }
    }
}

impl fmt::Display for AccessIdentityMode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Cloudflare Access environment surface. [lib/identity.js:14-38,57-77]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AccessConfig {
    /// `CF_ACCESS_TEAM_DOMAIN`, trimmed. [lib/identity.js:14]
    pub team_domain: String,
    /// `CF_ACCESS_AUD`, trimmed. [lib/identity.js:15]
    pub aud: String,
    /// `TRUST_ACCESS_HEADERS === "1"`. [lib/identity.js:16]
    pub trust_headers: bool,
    /// `REQUIRE_ACCESS_JWT === "1"`. [lib/identity.js:57]
    pub require_jwt: bool,
    /// `HEADER_TRUST_ALLOW_INSECURE === "1"`. [lib/identity.js:68]
    pub header_trust_allow_insecure: bool,
    /// `Number(ACCESS_CLOCK_TOLERANCE_S) || 60`. [lib/identity.js:22]
    pub clock_tolerance_seconds: u64,
    /// `ORG_EMAIL_DOMAINS` as `domain(lowercased) -> org`. [lib/identity.js:25-31]
    pub domain_orgs: BTreeMap<String, String>,
    /// `ADMIN_EMAILS`, trimmed and lowercased. [lib/identity.js:34-36]
    pub admin_emails: BTreeSet<String>,
    /// `ADMIN_EMAIL_DOMAINS`, trimmed and lowercased. [lib/identity.js:37-39]
    pub admin_email_domains: BTreeSet<String>,
}

impl AccessConfig {
    /// `JWT_VERIFICATION_ON = Boolean(TEAM_DOMAIN && AUD)` — [lib/identity.js:50]
    #[must_use]
    pub fn jwt_verification_on(&self) -> bool {
        !self.team_domain.is_empty() && !self.aud.is_empty()
    }

    /// `ACCESS_IDENTITY_MODE` — [lib/identity.js:51-55]
    #[must_use]
    pub fn identity_mode(&self) -> AccessIdentityMode {
        if self.jwt_verification_on() {
            AccessIdentityMode::Jwt
        } else if self.trust_headers {
            AccessIdentityMode::HeaderTrust
        } else {
            AccessIdentityMode::Disabled
        }
    }

    /// The Access JWKS endpoint. [lib/identity.js:43]
    #[must_use]
    pub fn jwks_url(&self) -> Option<String> {
        (!self.team_domain.is_empty()).then(|| {
            format!(
                "https://{}/cdn-cgi/access/certs",
                self.team_domain.trim_end_matches('/')
            )
        })
    }

    pub(super) fn from_source(env: &dyn EnvSource) -> Result<Self, AppError> {
        // `Number(x) || 60`: unset, empty, and 0 all select 60 in Node.
        let clock_tolerance_seconds = match present(env, "ACCESS_CLOCK_TOLERANCE_S") {
            None => DEFAULT_ACCESS_CLOCK_TOLERANCE_SECONDS,
            Some(raw) => {
                let trimmed = raw.trim();
                let parsed: u64 = trimmed.parse().map_err(|_| {
                    invalid(
                        "ACCESS_CLOCK_TOLERANCE_S",
                        trimmed,
                        "a whole number of seconds",
                    )
                })?;
                if parsed == 0 {
                    DEFAULT_ACCESS_CLOCK_TOLERANCE_SECONDS
                } else {
                    parsed
                }
            }
        };

        Ok(Self {
            team_domain: non_empty_string(env, "CF_ACCESS_TEAM_DOMAIN", ""),
            aud: non_empty_string(env, "CF_ACCESS_AUD", ""),
            trust_headers: flag(env, "TRUST_ACCESS_HEADERS"),
            require_jwt: flag(env, "REQUIRE_ACCESS_JWT"),
            header_trust_allow_insecure: flag(env, "HEADER_TRUST_ALLOW_INSECURE"),
            clock_tolerance_seconds,
            domain_orgs: parse_domain_orgs(env.get("ORG_EMAIL_DOMAINS").as_deref()),
            admin_emails: parse_lowercase_list(env.get("ADMIN_EMAILS").as_deref()),
            admin_email_domains: parse_lowercase_list(env.get("ADMIN_EMAIL_DOMAINS").as_deref()),
        })
    }
}

/// Port of the `DOMAIN_ORG` map builder — [lib/identity.js:25-31]
fn parse_domain_orgs(raw: Option<&str>) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    for pair in raw.unwrap_or_default().split(',') {
        let mut parts = pair.split(':').map(str::trim);
        let (Some(domain), Some(org)) = (parts.next(), parts.next()) else {
            continue;
        };
        if domain.is_empty() || org.is_empty() {
            continue;
        }
        // A later duplicate wins, matching `new Map(entries)`.
        map.insert(domain.to_ascii_lowercase(), org.to_owned());
    }
    map
}

/// Port of the `ADMIN_EMAILS`/`ADMIN_DOMAINS` set builders — [lib/identity.js:34-39]
fn parse_lowercase_list(raw: Option<&str>) -> BTreeSet<String> {
    raw.unwrap_or_default()
        .split(',')
        .map(|entry| entry.trim().to_ascii_lowercase())
        .filter(|entry| !entry.is_empty())
        .collect()
}

pub(super) fn parse_access_sync(
    env: &dyn EnvSource,
) -> Result<Option<crate::integrations::access_sync::AccessSyncConfig>, AppError> {
    if !disabled_unless_one(env, "ACCESS_SYNC_ENABLED")? {
        return Ok(None);
    }
    let identifier = |key: &str| -> Result<String, AppError> {
        let value = present(env, key).unwrap_or_default();
        let value = value.trim();
        if value.is_empty()
            || value.len() > 64
            || !value.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-')
        {
            return Err(AppError::Validation(format!(
                "{key} must be a Cloudflare identifier."
            )));
        }
        Ok(value.to_owned())
    };
    let token = present(env, "ACCESS_SYNC_API_TOKEN").unwrap_or_default();
    let token = token.trim();
    if token.is_empty() || token.len() > 512 || !token.bytes().all(|b| b.is_ascii_graphic()) {
        return Err(AppError::Validation(
            "ACCESS_SYNC_API_TOKEN must be a non-empty printable API token up to 512 bytes."
                .to_owned(),
        ));
    }
    Ok(Some(crate::integrations::access_sync::AccessSyncConfig {
        account_id: identifier("ACCESS_SYNC_ACCOUNT_ID")?,
        application_id: identifier("ACCESS_SYNC_APPLICATION_ID")?,
        policy_id: identifier("ACCESS_SYNC_POLICY_ID")?,
        api_token: Secret::new(token),
        hostname: url::Url::parse(&raw_string(env, "PUBLIC_BASE_URL", DEFAULT_PUBLIC_BASE_URL))
            .ok()
            .and_then(|url| url.host_str().map(str::to_owned))
            .ok_or_else(|| {
                AppError::Validation("PUBLIC_BASE_URL must have a hostname.".to_owned())
            })?,
    }))
}
