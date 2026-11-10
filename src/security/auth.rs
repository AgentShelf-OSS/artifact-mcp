//! Publisher bearer-key authentication.
//!
//! # Frozen behaviours
//!
//! | Behaviour |
//! |---|
//! | `Authorization: Bearer <key>` wins over `X-Api-Key` |
//! | A *matching* `Bearer` header short-circuits — `X-Api-Key` is never consulted, even when the captured key trims to empty |
//! | A non-matching `Authorization` (e.g. `Basic …`) falls through to `X-Api-Key` |
//! | Hash input is the UTF-8 bytes of the trimmed secret, hex-encoded SHA-256 |
//! | The seed path hashes the identical string |
//! | Revoked rows never authenticate (`revoked_at IS NULL`) |
//! | Failure is HTTP 401 with body message `unauthorized` |
//! | A key is an admin key **iff** its stored org is exactly `admin` |
//!
//! # Invariant 1 — org pinning
//!
//! A non-admin key's org is authoritative: an `org` argument can never redirect it. The only escape is a key whose stored org is literally `admin`, and only when it supplies a non-blank `org`. [`OrgTarget`] makes that structural — the requested org is consumed by a constructor that has the identity in hand, so no caller can assemble a target org without passing the check.
//!
//! Registration of the requested org is a *separate* check on the move path; it belongs to the `AdminService::org_exists` caller and is deliberately not duplicated here.
//!
//! # Secret containment
//!
//! [`BearerKey`] and [`KeyHash`] redact themselves in `Debug`/`Display`, and neither the key nor its hash is ever placed in an [`AppError`]. [`UNAUTHORIZED_MESSAGE`] is a constant, so a failed authentication cannot echo the presented credential.

use std::{fmt, sync::Arc};

use axum::http::{HeaderMap, header::AUTHORIZATION};
use sha2::{Digest as _, Sha256};

use crate::{
    error::AppError,
    model::{ClientId, OrgId, PublisherIdentity},
    ports::{BoxFuture, PublisherAuthenticator},
};

/// The stored org value that grants cross-tenant publisher rights.
pub const ADMIN_ORG: &str = "admin";

/// The frozen body message for a rejected publisher key.
pub const UNAUTHORIZED_MESSAGE: &str = "unauthorized";

/// The fallback credential header.
pub const API_KEY_HEADER: &str = "x-api-key";

/// The frozen message when an admin key omits the required explicit org.
pub const ADMIN_ORG_REQUIRED_MESSAGE: &str = "org is required for admin keys";

// ---------------------------------------------------------------------------
// Secret-carrying newtypes
// ---------------------------------------------------------------------------

/// A presented bearer credential.
///
/// Constructing one is the only way to reach [`BearerKey::hash`]; the value itself is never
/// rendered, so it cannot reach a log line or an error body by accident.
#[derive(Clone, PartialEq, Eq)]
pub struct BearerKey(String);

impl BearerKey {
    /// Wrap an already-extracted credential.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The SHA-256 hex digest stored in `api_keys.key_hash`.
    #[must_use]
    pub fn hash(&self) -> KeyHash {
        KeyHash(sha256_hex(&self.0))
    }

    /// Borrow the raw credential. Callers must not log the result.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for BearerKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("BearerKey(<redacted>)")
    }
}

impl fmt::Display for BearerKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("<redacted>")
    }
}

/// The hex SHA-256 of a bearer key, as stored in `api_keys.key_hash`.
///
/// A hash is password-equivalent here: the lookup is a plain equality match on an unsalted
/// digest, so leaking one leaks an authentication oracle. It redacts itself like the key.
#[derive(Clone, PartialEq, Eq)]
pub struct KeyHash(String);

impl KeyHash {
    /// Hash an arbitrary secret exactly as `sha256Hex` does.
    #[must_use]
    pub fn of(secret: &str) -> Self {
        Self(sha256_hex(secret))
    }

    /// Borrow the hex digest for a database comparison. Callers must not log the result.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for KeyHash {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("KeyHash(<redacted>)")
    }
}

impl fmt::Display for KeyHash {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("<redacted>")
    }
}

/// The hex SHA-256 of `value`'s UTF-8 bytes.
///
/// The stored hash format inherited from the retired Node twin uses UTF-8 input.
/// Both runtime lookup and the `ARTIFACT_API_KEYS` seed path use this function,
/// so seeded keys and existing stored keys authenticate with the same byte representation.
#[must_use]
pub fn sha256_hex(value: &str) -> String {
    use fmt::Write as _;
    let digest = Sha256::digest(value.as_bytes());
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        // Writing into a `String` cannot fail; the `Result` is discarded deliberately.
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

// ---------------------------------------------------------------------------
// Credential extraction
// ---------------------------------------------------------------------------

/// The characters JavaScript's `\s` and `String.prototype.trim` treat as whitespace.
///
/// HTTP header values reaching this function are visible ASCII plus space and tab, so only the
/// first members are reachable in practice; the rest are listed so the port is exact rather than
/// accidentally correct.
const fn is_js_whitespace(character: char) -> bool {
    matches!(character, '\u{2000}'..='\u{200a}')
        || matches!(
            character,
            '\u{9}'
                | '\u{a}'
                | '\u{b}'
                | '\u{c}'
                | '\u{d}'
                | '\u{20}'
                | '\u{a0}'
                | '\u{1680}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202f}'
                | '\u{205f}'
                | '\u{3000}'
                | '\u{feff}'
        )
}

/// `String.prototype.trim()`.
fn js_trim(value: &str) -> &str {
    value.trim_matches(is_js_whitespace)
}

/// `/^\s*Bearer\s+(.+?)\s*$/i` applied to `value`, returning capture group 1 already trimmed.
///
/// Returning `Some("")` is meaningful and not a quirk: `bearer()` returns from inside the
/// `if (m)` branch, so a header that *matches* but captures only whitespace still shadows
/// `X-Api-Key`. That is why this reports "matched" separately from "captured something".
///
/// The three conditions are exactly the regex's:
///
/// * `\s+` — at least one whitespace character follows the scheme;
/// * `(.+?)` — at least one further character exists (so a lone trailing space does not match);
/// * `.` never matches a line terminator, and `$` without the `m` flag only matches end-of-input,
///   so a line terminator inside the captured core makes the whole match fail. Header values
///   cannot carry those bytes, but reproducing the rule keeps the port exact rather than
///   accidentally correct.
fn match_bearer(value: &str) -> Option<&str> {
    let after_leading = value.trim_start_matches(is_js_whitespace);
    let scheme = after_leading.get(..6)?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let rest = &after_leading[6..];
    let mut characters = rest.chars();
    if !characters.next().is_some_and(is_js_whitespace) || characters.next().is_none() {
        return None;
    }
    let captured = js_trim(rest);
    if captured.contains(['\n', '\r', '\u{2028}', '\u{2029}']) {
        return None;
    }
    Some(captured)
}

/// The presented credential, or `None` when there is none.
///
/// Returns `None` for an empty credential so the "no credential" branch cannot be forgotten.
/// A header whose bytes are not valid UTF-8 is treated as absent. This preserves the retired
/// Node twin's refusal of headers that could not match any stored hash.
#[must_use]
pub fn bearer_key(headers: &HeaderMap) -> Option<BearerKey> {
    let authorization = headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();

    // A matching `Bearer` header returns immediately, so `X-Api-Key` is unreachable behind it.
    if let Some(key) = match_bearer(authorization) {
        return (!key.is_empty()).then(|| BearerKey::new(key));
    }

    let fallback = headers
        .get(API_KEY_HEADER)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    let key = js_trim(fallback);
    (!key.is_empty()).then(|| BearerKey::new(key))
}

// ---------------------------------------------------------------------------
// Key directory
// ---------------------------------------------------------------------------

/// One active `api_keys` row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublisherKeyRecord {
    /// `api_keys.client_id`.
    pub client_id: ClientId,
    /// `api_keys.org`.
    pub org: OrgId,
    /// `api_keys.label`, with `NULL` collapsed to `""` exactly as `row.label || ""` does.
    pub label: String,
    /// `api_keys.role`, defaulting to `author` for every pre-v22 key.
    pub role: String,
}

/// The narrow lookup [`KeyAuthenticator`] needs.
///
/// The frozen `PublisherAuthenticator` port takes only a [`HeaderMap`], so the database
/// dependency enters through this narrow interface, implemented by `persistence::keys`.
/// Production startup injects the implementation. This also keeps the
/// authenticator's tests free of a database.
pub trait PublisherKeyDirectory: Send + Sync + fmt::Debug {
    /// The single active row whose `key_hash` equals `hash`, or `None`.
    ///
    /// Implementations must apply `revoked_at IS NULL`.
    ///
    /// # Errors
    /// Returns [`AppError`] when the lookup itself fails; a *missing* row is `Ok(None)`.
    fn find_active<'a>(
        &'a self,
        hash: &'a KeyHash,
    ) -> BoxFuture<'a, Result<Option<PublisherKeyRecord>, AppError>>;
}

/// A directory with no keys at all: every authentication attempt fails closed.
///
/// Used by bootstrap and by tests that must prove the unauthenticated path.
#[derive(Clone, Copy, Debug, Default)]
pub struct EmptyKeyDirectory;

impl PublisherKeyDirectory for EmptyKeyDirectory {
    fn find_active<'a>(
        &'a self,
        _hash: &'a KeyHash,
    ) -> BoxFuture<'a, Result<Option<PublisherKeyRecord>, AppError>> {
        Box::pin(async { Ok(None) })
    }
}

// ---------------------------------------------------------------------------
// Authenticator
// ---------------------------------------------------------------------------

/// `checkKey(req)` as the frozen [`PublisherAuthenticator`] port.
#[derive(Clone, Debug)]
pub struct KeyAuthenticator {
    directory: Arc<dyn PublisherKeyDirectory>,
}

impl KeyAuthenticator {
    /// Authenticate against `directory`.
    #[must_use]
    pub const fn new(directory: Arc<dyn PublisherKeyDirectory>) -> Self {
        Self { directory }
    }

    /// The rejection every failure path shares.
    ///
    /// One constant for "no credential", "unknown credential" and "revoked credential" means a
    /// caller cannot distinguish them, and means no failure message can carry the credential.
    #[must_use]
    pub fn unauthorized() -> AppError {
        AppError::Unauthorized(UNAUTHORIZED_MESSAGE.to_owned())
    }
}

impl PublisherAuthenticator for KeyAuthenticator {
    fn authenticate<'a>(
        &'a self,
        headers: &'a HeaderMap,
    ) -> BoxFuture<'a, Result<PublisherIdentity, AppError>> {
        Box::pin(async move {
            let key = bearer_key(headers).ok_or_else(Self::unauthorized)?;
            let record = self
                .directory
                .find_active(&key.hash())
                .await?
                .ok_or_else(Self::unauthorized)?;
            identity_for(record)
        })
    }
}

/// Build the frozen [`PublisherIdentity`] from an active row.
///
/// `is_admin` is derived, never stored: the key row has no admin column, and administrator
/// status depends on `auth.org === "admin"`.
/// # Errors
/// Rejects an unknown stored role with the standard unauthorized response.
pub fn identity_for(record: PublisherKeyRecord) -> Result<PublisherIdentity, AppError> {
    // Admin status is derived by `PublisherIdentity::is_admin()` from `org == "admin"`, so there is
    // no flag to set (and no way for a caller to set it inconsistently).
    let role = record
        .role
        .parse()
        .map_err(|error: crate::model::RoleParseError| {
            tracing::warn!(%error, "rejecting publisher key with invalid stored role");
            KeyAuthenticator::unauthorized()
        })?;
    Ok(PublisherIdentity {
        client_id: record.client_id,
        org: record.org,
        label: record.label,
        role,
        scopes: None,
    })
}

// ---------------------------------------------------------------------------
// Invariant 1 — org pinning
// ---------------------------------------------------------------------------

/// The org a publisher operation is allowed to act on.
///
/// The only constructors take the authenticated identity, so an `org` argument can never reach an
/// operation without having been filtered through the identity that supplied it. A non-admin key
/// always yields its own org.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OrgTarget(OrgId);

impl OrgTarget {
    /// `auth.org === "admin" && args.org?.trim() ? args.org.trim() : auth.org`
    ///
    /// This is the publish/update shape: a blank or absent request org silently falls back to the
    /// key's own org, and a non-admin key's request org is discarded outright.
    #[must_use]
    pub fn pinned(identity: &PublisherIdentity, requested: Option<&str>) -> Self {
        let requested = requested.map(str::trim).filter(|org| !org.is_empty());
        match requested {
            Some(org) if identity.is_admin() => Self(OrgId(org.to_owned())),
            _ => Self(identity.org.clone()),
        }
    }

    /// `auth.org === "admin" ? args.org.trim() : auth.org` with the admin blank check
    ///
    /// This is the category shape: an admin key **must** name an org, because `admin` is not a
    /// real tenant it could fall back to.
    ///
    /// # Errors
    /// Returns [`AppError::Validation`] carrying [`ADMIN_ORG_REQUIRED_MESSAGE`] when an admin key
    /// supplies no org.
    pub fn explicit(
        identity: &PublisherIdentity,
        requested: Option<&str>,
    ) -> Result<Self, AppError> {
        if !identity.is_admin() {
            return Ok(Self(identity.org.clone()));
        }
        let requested = requested.map(str::trim).filter(|org| !org.is_empty());
        requested.map_or_else(
            || Err(AppError::Validation(ADMIN_ORG_REQUIRED_MESSAGE.to_owned())),
            |org| Ok(Self(OrgId(org.to_owned()))),
        )
    }

    /// Borrow the resolved org.
    #[must_use]
    pub const fn org(&self) -> &OrgId {
        &self.0
    }

    /// Consume the wrapper.
    #[must_use]
    pub fn into_org(self) -> OrgId {
        self.0
    }
}
