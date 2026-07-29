//! OAuth 2.0 service-credential authentication for the MCP transport.
//!
//! The authorization server issues the access token; ArtifactShelf is only the protected
//! resource. Tokens are never rendered or logged, JWT algorithms are explicitly allowlisted,
//! and every accepted token carries an explicit scope set.

use std::{collections::BTreeSet, fmt, sync::Arc};

use axum::http::{HeaderMap, header::AUTHORIZATION};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD as BASE64URL};
use serde_json::{Map, Value};

use crate::{
    config::{Clock, OAuthConfig, SystemClock},
    error::AppError,
    model::{ClientId, OrgId, PublisherIdentity},
    ports::{BoxFuture, PublisherAuthenticator},
    security::{
        auth::{KeyAuthenticator, UNAUTHORIZED_MESSAGE},
        jwks::JwksProvider,
    },
};

pub const OAUTH_EXTENSION: &str = "io.modelcontextprotocol/oauth-client-credentials";
pub const SCOPE_READ: &str = "artifacts:read";
pub const SCOPE_PUBLISH: &str = "artifacts:publish";
pub const SCOPE_REVIEW: &str = "artifacts:review";
pub const SCOPE_VISIBILITY: &str = "artifacts:visibility";
pub const SCOPE_DELETE: &str = "artifacts:delete";
pub const SUPPORTED_SCOPES: [&str; 5] = [
    SCOPE_READ,
    SCOPE_PUBLISH,
    SCOPE_REVIEW,
    SCOPE_VISIBILITY,
    SCOPE_DELETE,
];

#[derive(Clone, PartialEq, Eq)]
pub struct OAuthAccessToken(String);

impl OAuthAccessToken {
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for OAuthAccessToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("OAuthAccessToken(<redacted>)")
    }
}

impl fmt::Display for OAuthAccessToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("<redacted>")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OAuthTokenRejection {
    Missing,
    Malformed,
    UnsupportedHeader,
    DisallowedAlgorithm,
    NoVerificationKey,
    BadSignature,
    WrongIssuer,
    WrongAudience,
    MissingClaim(&'static str),
    InvalidClaim(&'static str),
    NotYetValid,
    Expired,
    LifetimeTooLong,
}

#[derive(Clone, Debug)]
pub struct OAuthAuthenticator {
    config: OAuthConfig,
    jwks: Arc<dyn JwksProvider>,
    clock: Arc<dyn Clock>,
}

impl OAuthAuthenticator {
    #[must_use]
    pub fn new(config: OAuthConfig, jwks: Arc<dyn JwksProvider>) -> Self {
        Self::with_clock(config, jwks, Arc::new(SystemClock))
    }

    #[must_use]
    pub fn with_clock(
        config: OAuthConfig,
        jwks: Arc<dyn JwksProvider>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            config,
            jwks,
            clock,
        }
    }

    /// Verify one signed access token and map its trusted claims into the existing publisher
    /// tenant/role model. No raw claim set escapes this boundary.
    pub async fn verify(
        &self,
        token: &OAuthAccessToken,
    ) -> Result<PublisherIdentity, OAuthTokenRejection> {
        let mut segments = token.expose().split('.');
        let (Some(protected), Some(payload), Some(signature), None) = (
            segments.next(),
            segments.next(),
            segments.next(),
            segments.next(),
        ) else {
            return Err(OAuthTokenRejection::Malformed);
        };
        let header = decode_json_object(protected).ok_or(OAuthTokenRejection::Malformed)?;
        if header.contains_key("crit") || header.get("b64") == Some(&Value::Bool(false)) {
            return Err(OAuthTokenRejection::UnsupportedHeader);
        }
        let algorithm = required_string_claim(&header, "alg")?;
        if !self.config.allowed_algorithms.contains(algorithm) {
            return Err(OAuthTokenRejection::DisallowedAlgorithm);
        }
        let kid = header.get("kid").and_then(Value::as_str);
        let key = self
            .jwks
            .resolve(algorithm, kid)
            .await
            .map_err(|_| OAuthTokenRejection::NoVerificationKey)?;
        let signing_input = format!("{protected}.{payload}");
        let verified = jsonwebtoken::crypto::verify(
            signature,
            signing_input.as_bytes(),
            key.key(),
            key.algorithm(),
        )
        .unwrap_or(false);
        if !verified {
            return Err(OAuthTokenRejection::BadSignature);
        }

        let claims = decode_json_object(payload).ok_or(OAuthTokenRejection::Malformed)?;
        if required_string_claim(&claims, "iss")? != self.config.issuer {
            return Err(OAuthTokenRejection::WrongIssuer);
        }
        if !audience_matches(claims.get("aud"), &self.config.audience) {
            return Err(if claims.contains_key("aud") {
                OAuthTokenRejection::WrongAudience
            } else {
                OAuthTokenRejection::MissingClaim("aud")
            });
        }

        let now = self.clock.now_unix_seconds();
        let tolerance = i64::try_from(self.config.clock_tolerance_seconds).unwrap_or(i64::MAX);
        let issued_at = required_integer_claim(&claims, "iat")?;
        let expires = required_integer_claim(&claims, "exp")?;
        if issued_at > now.saturating_add(tolerance) {
            return Err(OAuthTokenRejection::NotYetValid);
        }
        if let Some(not_before) = optional_integer_claim(&claims, "nbf")?
            && not_before > now.saturating_add(tolerance)
        {
            return Err(OAuthTokenRejection::NotYetValid);
        }
        if expires <= now.saturating_sub(tolerance) {
            return Err(OAuthTokenRejection::Expired);
        }
        let max_lifetime =
            i64::try_from(self.config.max_token_lifetime_seconds).unwrap_or(i64::MAX);
        if expires <= issued_at || expires.saturating_sub(issued_at) > max_lifetime {
            return Err(OAuthTokenRejection::LifetimeTooLong);
        }

        let subject = claims.get("sub").and_then(Value::as_str);
        let client_id = claims.get("client_id").and_then(Value::as_str);
        if let (Some(subject), Some(client_id)) = (subject, client_id)
            && subject != client_id
        {
            return Err(OAuthTokenRejection::InvalidClaim("client_id"));
        }
        let client_id = client_id
            .or(subject)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or(OAuthTokenRejection::MissingClaim("sub"))?;
        let org = required_string_claim(&claims, "org")?
            .trim()
            .to_ascii_lowercase();
        if org.is_empty() {
            return Err(OAuthTokenRejection::InvalidClaim("org"));
        }
        let role = claims
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("author");
        if !matches!(role, "reader" | "author" | "collaborator") {
            return Err(OAuthTokenRejection::InvalidClaim("role"));
        }
        let label = claims
            .get("client_name")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or(client_id)
            .chars()
            .take(120)
            .collect();

        Ok(PublisherIdentity {
            client_id: ClientId::from(client_id),
            org: OrgId::from(org),
            label,
            role: role.to_owned(),
            scopes: Some(scope_set(&claims)?),
        })
    }
}

impl PublisherAuthenticator for OAuthAuthenticator {
    fn authenticate<'a>(
        &'a self,
        headers: &'a HeaderMap,
    ) -> BoxFuture<'a, Result<PublisherIdentity, AppError>> {
        Box::pin(async move {
            let token = oauth_bearer(headers).ok_or_else(unauthorized)?;
            self.verify(&token).await.map_err(|_| unauthorized())
        })
    }
}

/// API-key compatibility followed by OAuth verification. Only an ordinary unauthorized API-key
/// miss falls through; database failures remain failures.
#[derive(Clone, Debug)]
pub struct CompositePublisherAuthenticator {
    api_keys: Option<KeyAuthenticator>,
    oauth: Option<OAuthAuthenticator>,
}

impl CompositePublisherAuthenticator {
    #[must_use]
    pub const fn new(
        api_keys: Option<KeyAuthenticator>,
        oauth: Option<OAuthAuthenticator>,
    ) -> Self {
        Self { api_keys, oauth }
    }
}

impl PublisherAuthenticator for CompositePublisherAuthenticator {
    fn authenticate<'a>(
        &'a self,
        headers: &'a HeaderMap,
    ) -> BoxFuture<'a, Result<PublisherIdentity, AppError>> {
        Box::pin(async move {
            if let Some(api_keys) = &self.api_keys {
                match api_keys.authenticate(headers).await {
                    Ok(identity) => return Ok(identity),
                    Err(AppError::Unauthorized(_)) => {}
                    Err(error) => return Err(error),
                }
            }
            if let Some(oauth) = &self.oauth {
                return oauth.authenticate(headers).await;
            }
            Err(unauthorized())
        })
    }
}

#[must_use]
pub fn required_scope(method: &str, name: Option<&str>) -> Option<&'static str> {
    match method {
        "resources/list" | "resources/templates/list" | "resources/read" | "tasks/get" => {
            Some(SCOPE_READ)
        }
        "tasks/update" | "tasks/cancel" => Some(SCOPE_PUBLISH),
        "tools/call" => match name {
            Some(
                "list_artifacts" | "read_artifact" | "list_categories" | "list_revisions"
                | "list_shares" | "artifact_stats",
            ) => Some(SCOPE_READ),
            Some(
                "publish_artifact"
                | "publish_bundle"
                | "update_artifact"
                | "patch_artifact"
                | "set_category"
                | "create_category"
                | "delete_category"
                | "restore_artifact"
                | "regenerate_artifact_preview",
            ) => Some(SCOPE_PUBLISH),
            Some("list_feedback" | "resolve_feedback" | "reopen_feedback" | "submit_feedback") => {
                Some(SCOPE_REVIEW)
            }
            Some("set_visibility" | "create_share" | "revoke_share") => Some(SCOPE_VISIBILITY),
            Some("delete_artifact") => Some(SCOPE_DELETE),
            _ => None,
        },
        _ => None,
    }
}

fn oauth_bearer(headers: &HeaderMap) -> Option<OAuthAccessToken> {
    let value = headers.get(AUTHORIZATION)?.to_str().ok()?.trim();
    let (scheme, credential) = value.split_once(char::is_whitespace)?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let credential = credential.trim();
    (!credential.is_empty()).then(|| OAuthAccessToken::new(credential))
}

fn unauthorized() -> AppError {
    AppError::Unauthorized(UNAUTHORIZED_MESSAGE.to_owned())
}

fn decode_json_object(segment: &str) -> Option<Map<String, Value>> {
    let bytes = BASE64URL.decode(segment).ok()?;
    serde_json::from_slice::<Value>(&bytes)
        .ok()?
        .as_object()
        .cloned()
}

fn required_string_claim<'a>(
    claims: &'a Map<String, Value>,
    name: &'static str,
) -> Result<&'a str, OAuthTokenRejection> {
    claims
        .get(name)
        .ok_or(OAuthTokenRejection::MissingClaim(name))?
        .as_str()
        .filter(|value| !value.is_empty())
        .ok_or(OAuthTokenRejection::InvalidClaim(name))
}

fn required_integer_claim(
    claims: &Map<String, Value>,
    name: &'static str,
) -> Result<i64, OAuthTokenRejection> {
    claims
        .get(name)
        .ok_or(OAuthTokenRejection::MissingClaim(name))?
        .as_i64()
        .ok_or(OAuthTokenRejection::InvalidClaim(name))
}

fn optional_integer_claim(
    claims: &Map<String, Value>,
    name: &'static str,
) -> Result<Option<i64>, OAuthTokenRejection> {
    claims
        .get(name)
        .map(|value| {
            value
                .as_i64()
                .ok_or(OAuthTokenRejection::InvalidClaim(name))
        })
        .transpose()
}

fn audience_matches(claim: Option<&Value>, expected: &str) -> bool {
    match claim {
        Some(Value::String(value)) => value == expected,
        Some(Value::Array(values)) => values.iter().any(|value| value.as_str() == Some(expected)),
        _ => false,
    }
}

fn scope_set(claims: &Map<String, Value>) -> Result<BTreeSet<String>, OAuthTokenRejection> {
    if let Some(scope) = claims.get("scope") {
        let scope = scope
            .as_str()
            .ok_or(OAuthTokenRejection::InvalidClaim("scope"))?;
        return Ok(scope.split_ascii_whitespace().map(str::to_owned).collect());
    }
    match claims.get("scp") {
        None => Ok(BTreeSet::new()),
        Some(Value::String(scope)) => {
            Ok(scope.split_ascii_whitespace().map(str::to_owned).collect())
        }
        Some(Value::Array(scopes)) => scopes
            .iter()
            .map(|scope| {
                scope
                    .as_str()
                    .map(str::to_owned)
                    .ok_or(OAuthTokenRejection::InvalidClaim("scp"))
            })
            .collect(),
        Some(_) => Err(OAuthTokenRejection::InvalidClaim("scp")),
    }
}
