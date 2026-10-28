//! MCP OAuth service-credential configuration.

use std::collections::BTreeSet;

use crate::error::AppError;

use super::{
    defaults::{DEFAULT_OAUTH_CLOCK_TOLERANCE_SECONDS, DEFAULT_OAUTH_MAX_TOKEN_LIFETIME_SECONDS},
    env::EnvSource,
    parse::{enabled_unless_zero, non_empty_string, positive_integer, present},
};

/// Optional OAuth resource-server configuration for MCP machine identities.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OAuthConfig {
    /// Exact access-token issuer.
    pub issuer: String,
    /// Exact MCP resource audience.
    pub audience: String,
    /// Bounded/cached JSON Web Key Set endpoint.
    pub jwks_url: String,
    /// Explicit asymmetric JWS allowlist.
    pub allowed_algorithms: BTreeSet<String>,
    /// Temporal-claim clock tolerance.
    pub clock_tolerance_seconds: u64,
    /// Maximum accepted `exp - iat` window.
    pub max_token_lifetime_seconds: u64,
    /// Compatibility switch for database-backed API keys.
    pub api_keys_enabled: bool,
    /// Permit HTTP OAuth endpoints only when each HTTP endpoint is a canonical loopback host.
    pub allow_loopback_http: bool,
}

impl Default for OAuthConfig {
    fn default() -> Self {
        Self {
            issuer: String::new(),
            audience: String::new(),
            jwks_url: String::new(),
            allowed_algorithms: BTreeSet::from(["RS256".to_owned()]),
            clock_tolerance_seconds: DEFAULT_OAUTH_CLOCK_TOLERANCE_SECONDS,
            max_token_lifetime_seconds: DEFAULT_OAUTH_MAX_TOKEN_LIFETIME_SECONDS,
            api_keys_enabled: true,
            allow_loopback_http: false,
        }
    }
}

impl OAuthConfig {
    /// OAuth is enabled only by a complete issuer/audience/JWKS triple.
    #[must_use]
    pub fn enabled(&self) -> bool {
        !self.issuer.is_empty() && !self.audience.is_empty() && !self.jwks_url.is_empty()
    }

    pub(super) fn from_source(env: &dyn EnvSource) -> Result<Self, AppError> {
        let issuer = non_empty_string(env, "MCP_OAUTH_ISSUER", "");
        let audience = non_empty_string(env, "MCP_OAUTH_AUDIENCE", "");
        let jwks_url = non_empty_string(env, "MCP_OAUTH_JWKS_URL", "");
        let configured = [
            !issuer.is_empty(),
            !audience.is_empty(),
            !jwks_url.is_empty(),
        ]
        .into_iter()
        .filter(|present| *present)
        .count();
        if configured != 0 && configured != 3 {
            return Err(AppError::Validation(
                "MCP OAuth requires MCP_OAUTH_ISSUER, MCP_OAUTH_AUDIENCE, and MCP_OAUTH_JWKS_URL together"
                    .to_owned(),
            ));
        }
        // This development-only exception is intentionally ignored when OAuth is disabled, so
        // API-key-only deployments keep accepting unrelated environment variables. Once the
        // complete OAuth triple is present, the switch itself is strict.
        let allow_loopback_http = if configured == 3 {
            strict_oauth_loopback_opt_in(env)?
        } else {
            false
        };
        if configured == 3 {
            validate_oauth_url("MCP_OAUTH_ISSUER", &issuer, allow_loopback_http)?;
            validate_oauth_url("MCP_OAUTH_JWKS_URL", &jwks_url, allow_loopback_http)?;
        }

        let algorithms = present(env, "MCP_OAUTH_ALLOWED_ALGS").map_or_else(
            || vec!["RS256".to_owned()],
            |raw| {
                raw.split(',')
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_owned)
                    .collect()
            },
        );
        const SUPPORTED: [&str; 10] = [
            "RS256", "RS384", "RS512", "PS256", "PS384", "PS512", "ES256", "ES384", "EdDSA",
            "Ed25519",
        ];
        if algorithms.is_empty()
            || algorithms
                .iter()
                .any(|algorithm| !SUPPORTED.contains(&algorithm.as_str()))
        {
            return Err(AppError::Validation(
                "MCP_OAUTH_ALLOWED_ALGS must contain only supported asymmetric JWS algorithms"
                    .to_owned(),
            ));
        }

        Ok(Self {
            issuer,
            audience,
            jwks_url,
            allowed_algorithms: algorithms.into_iter().collect(),
            clock_tolerance_seconds: positive_integer(
                env,
                "MCP_OAUTH_CLOCK_TOLERANCE_S",
                DEFAULT_OAUTH_CLOCK_TOLERANCE_SECONDS,
            )?,
            max_token_lifetime_seconds: positive_integer(
                env,
                "MCP_OAUTH_MAX_TOKEN_LIFETIME_S",
                DEFAULT_OAUTH_MAX_TOKEN_LIFETIME_SECONDS,
            )?,
            api_keys_enabled: enabled_unless_zero(env, "MCP_API_KEYS_ENABLED")?,
            allow_loopback_http,
        })
    }
}

fn validate_oauth_url(key: &str, value: &str, allow_loopback_http: bool) -> Result<(), AppError> {
    let parsed = url::Url::parse(value).map_err(|_| {
        AppError::Validation(format!(
            "{key} must be an absolute HTTPS URL (or canonical loopback HTTP URL with MCP_OAUTH_ALLOW_LOOPBACK_HTTP=1)"
        ))
    })?;
    let host = parsed.host();
    let is_loopback = host.as_ref().is_some_and(|host| match host {
        url::Host::Domain(domain) => domain.eq_ignore_ascii_case("localhost"),
        url::Host::Ipv4(address) => address.is_loopback(),
        url::Host::Ipv6(address) => *address == std::net::Ipv6Addr::LOCALHOST,
    });
    let secure = parsed.scheme() == "https";
    let loopback_http = parsed.scheme() == "http" && allow_loopback_http && is_loopback;
    if !(secure || loopback_http) {
        return Err(AppError::Validation(format!(
            "{key} must use HTTPS; HTTP is allowed only for canonical loopback hosts when MCP_OAUTH_ALLOW_LOOPBACK_HTTP=1"
        )));
    }
    let authority_has_userinfo = value
        .find("://")
        .and_then(|scheme_end| value.get(scheme_end + 3..))
        .and_then(|authority| {
            authority
                .find(['/', '?', '#'])
                .map_or(Some(authority), |end| authority.get(..end))
        })
        .is_some_and(|authority| authority.contains('@'));
    if host.is_none()
        || parsed.fragment().is_some()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || authority_has_userinfo
    {
        return Err(AppError::Validation(format!(
            "{key} must be an absolute HTTPS URL without credentials or a fragment"
        )));
    }
    Ok(())
}

/// Strict opt-in switch for development-only loopback HTTP. Unlike the legacy compatibility
/// switches, this defaults to false and never treats an unrecognised value as enabled.
fn strict_oauth_loopback_opt_in(env: &dyn EnvSource) -> Result<bool, AppError> {
    match env.get("MCP_OAUTH_ALLOW_LOOPBACK_HTTP").as_deref() {
        None | Some("") | Some("0") => Ok(false),
        Some(raw) if raw.trim().is_empty() => Ok(false),
        Some("1") => Ok(true),
        Some(_) => Err(AppError::Validation(
            "MCP_OAUTH_ALLOW_LOOPBACK_HTTP must be \"0\" or \"1\"".to_owned(),
        )),
    }
}
