//! Owned by U05 (sol) — JWKS retrieval, caching, and rotation.
//!
//! Authority: `lib/identity.js:42-47` chooses `jose`'s `createRemoteJWKSet`, so the *behaviour*
//! this module must reproduce lives in `jose@5.10.0`:
//!
//! * `node_modules/jose/dist/node/esm/jwks/remote.js` — cache freshness, cooldown, rotation.
//! * `node_modules/jose/dist/node/esm/jwks/local.js` — candidate filtering and key import.
//!
//! # Frozen behaviours
//!
//! | Behaviour | Value | jose authority |
//! |---|---|---|
//! | Endpoint | `https://<team>/cdn-cgi/access/certs` | `lib/identity.js:44` |
//! | Cache max age | 600 000 ms | `remote.js` constructor |
//! | Refetch cooldown | 30 000 ms | `remote.js` constructor |
//! | Fetch timeout | 5 000 ms | `remote.js` constructor |
//! | Reload trigger | no local set, or the set is older than the max age | `remote.js#getKey` |
//! | Rotation | a *no matching key* result reloads once, unless cooling down | `remote.js#getKey` |
//! | `kty` | derived from the token's `alg` prefix, never from the key | `local.js#getKtyFromAlg` |
//! | Candidate filter | `kty`, then `kid` (when the header carries one), then the key's own `alg`, then `use === "sig"`, then `key_ops ∋ "verify"`, then the curve | `local.js#getKey` |
//! | Ambiguity | **more than one** candidate is an error, not a "pick the first" | `local.js#getKey` |
//!
//! Exactly reproducing "unknown `alg` prefix is an error, not a miss" matters: it means an
//! `alg: "none"` or `alg: "HS256"` token can never trigger a JWKS reload, and can never select an
//! asymmetric key to be misused as an HMAC secret.
//!
//! # Deliberate hardening
//!
//! `jose` reads the JWKS response body without a size limit. [`HttpJwksSource`] caps it at
//! [`MAX_JWKS_BYTES`]. A real Cloudflare Access JWKS is a few hundred bytes, so the cap cannot
//! change any accept/reject decision; it only bounds memory if the endpoint is hostile.

use std::{fmt, sync::Arc, time::Duration};

use jsonwebtoken::{Algorithm, DecodingKey};
use serde_json::Value;
use tokio::sync::Mutex;

use crate::{
    config::{Clock, SystemClock},
    error::AppError,
    ports::BoxFuture,
};

/// `cacheMaxAge` default, in milliseconds. [jose `jwks/remote.js`]
pub const CACHE_MAX_AGE_MS: i64 = 600_000;
/// `cooldownDuration` default, in milliseconds. [jose `jwks/remote.js`]
pub const COOLDOWN_MS: i64 = 30_000;
/// `timeoutDuration` default, in milliseconds. [jose `jwks/remote.js`]
pub const FETCH_TIMEOUT_MS: u64 = 5_000;
/// Upper bound on a JWKS response body. Hardening beyond `jose`; see the module docs.
pub const MAX_JWKS_BYTES: usize = 1024 * 1024;

/// Message for a token whose `alg`/`kid` names no key in the set. Contains no token material.
pub const NO_MATCHING_KEY_MESSAGE: &str = "no matching Cloudflare Access signing key";
/// Message for an ambiguous key set.
pub const MULTIPLE_MATCHING_KEYS_MESSAGE: &str = "multiple Cloudflare Access signing keys match";
/// Message for an `alg` outside the JWS families a JWKS can describe.
pub const UNSUPPORTED_ALGORITHM_MESSAGE: &str = "unsupported Cloudflare Access token algorithm";
/// Message for a malformed key set.
pub const MALFORMED_JWKS_MESSAGE: &str = "Cloudflare Access JWKS is malformed";
/// Message for an unreachable or failing endpoint.
pub const JWKS_UNAVAILABLE_MESSAGE: &str = "Cloudflare Access JWKS is unavailable";

// ---------------------------------------------------------------------------
// Key set model
// ---------------------------------------------------------------------------

/// One JSON Web Key, kept as its raw members.
///
/// `jose` filters on the *parsed JSON*, not on a typed model, so an unrecognised `kty`, an
/// unexpected member, or a `key_ops` that is not an array simply fails to match instead of
/// invalidating the whole set. Holding the raw object reproduces that; a typed struct would
/// reject sets `jose` accepts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JwkKey {
    members: serde_json::Map<String, Value>,
}

impl JwkKey {
    /// Wrap a JSON object as a key.
    #[must_use]
    pub const fn new(members: serde_json::Map<String, Value>) -> Self {
        Self { members }
    }

    /// A string-valued member, or `None` when absent or another type.
    #[must_use]
    pub fn string(&self, name: &str) -> Option<&str> {
        self.members.get(name).and_then(Value::as_str)
    }

    /// `kid`.
    #[must_use]
    pub fn kid(&self) -> Option<&str> {
        self.string("kid")
    }

    /// `kty`.
    #[must_use]
    pub fn kty(&self) -> Option<&str> {
        self.string("kty")
    }

    /// `alg`.
    #[must_use]
    pub fn alg(&self) -> Option<&str> {
        self.string("alg")
    }

    /// `Array.isArray(jwk.key_ops) ? jwk.key_ops.includes("verify") : true` — the filter is only
    /// applied when the member really is an array. [jose `jwks/local.js`]
    fn allows_verify(&self) -> bool {
        match self.members.get("key_ops") {
            Some(Value::Array(operations)) => operations
                .iter()
                .any(|operation| operation.as_str() == Some("verify")),
            _ => true,
        }
    }

    /// The curve constraint `jose` applies for the EC and OKP algorithms.
    fn curve_matches(&self, algorithm: &str) -> bool {
        let required: &[&str] = match algorithm {
            "ES256" => &["P-256"],
            "ES256K" => &["secp256k1"],
            "ES384" => &["P-384"],
            "ES512" => &["P-521"],
            "Ed25519" => &["Ed25519"],
            "EdDSA" => &["Ed25519", "Ed448"],
            _ => return true,
        };
        self.string("crv")
            .is_some_and(|curve| required.contains(&curve))
    }

    /// Build a verification key for `algorithm`.
    ///
    /// Mirrors `DecodingKey::from_jwk` but reads the raw members, so a set member missing its
    /// component parameters fails closed instead of failing to parse the whole document.
    fn decoding_key(&self, algorithm: Algorithm) -> Result<DecodingKey, AppError> {
        let malformed = || AppError::Unauthorized(MALFORMED_JWKS_MESSAGE.to_owned());
        let component = |name: &str| self.string(name).ok_or_else(malformed);
        let key = match self.kty() {
            Some("RSA") => DecodingKey::from_rsa_components(component("n")?, component("e")?),
            Some("EC") => DecodingKey::from_ec_components(component("x")?, component("y")?),
            Some("OKP") => DecodingKey::from_ed_components(component("x")?),
            // `kty` was already matched against the algorithm family, so this is unreachable for
            // a selected key; it stays a fail-closed error rather than a panic.
            _ => return Err(malformed()),
        }
        .map_err(|_| malformed())?;
        debug_assert_eq!(key.family(), algorithm_family(algorithm));
        Ok(key)
    }
}

const fn algorithm_family(algorithm: Algorithm) -> jsonwebtoken::AlgorithmFamily {
    use jsonwebtoken::AlgorithmFamily as Family;
    match algorithm {
        Algorithm::HS256 | Algorithm::HS384 | Algorithm::HS512 => Family::Hmac,
        Algorithm::ES256 | Algorithm::ES384 => Family::Ec,
        Algorithm::EdDSA => Family::Ed,
        Algorithm::RS256
        | Algorithm::RS384
        | Algorithm::RS512
        | Algorithm::PS256
        | Algorithm::PS384
        | Algorithm::PS512 => Family::Rsa,
    }
}

/// A parsed `{ "keys": [ … ] }` document.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct JwkDocument {
    keys: Vec<JwkKey>,
}

impl JwkDocument {
    /// Parse a key set exactly as `jose`'s `LocalJWKSet` constructor validates one.
    ///
    /// # Errors
    /// Returns [`AppError::Unauthorized`] with [`MALFORMED_JWKS_MESSAGE`] when the document is
    /// not an object, has no `keys` array, or any member of that array is not an object —
    /// `jose` throws `JWKSInvalid` in all three cases.
    pub fn from_json(value: &Value) -> Result<Self, AppError> {
        let malformed = || AppError::Unauthorized(MALFORMED_JWKS_MESSAGE.to_owned());
        let entries = value
            .as_object()
            .and_then(|document| document.get("keys"))
            .and_then(Value::as_array)
            .ok_or_else(malformed)?;
        let mut keys = Vec::with_capacity(entries.len());
        for entry in entries {
            let members = entry.as_object().ok_or_else(malformed)?;
            keys.push(JwkKey::new(members.clone()));
        }
        Ok(Self { keys })
    }

    /// Parse a key set from raw bytes.
    ///
    /// # Errors
    /// Returns [`AppError::Unauthorized`] with [`MALFORMED_JWKS_MESSAGE`] for invalid JSON or a
    /// document that fails [`JwkDocument::from_json`].
    pub fn from_slice(bytes: &[u8]) -> Result<Self, AppError> {
        let value: Value = serde_json::from_slice(bytes)
            .map_err(|_| AppError::Unauthorized(MALFORMED_JWKS_MESSAGE.to_owned()))?;
        Self::from_json(&value)
    }

    /// The keys, in document order.
    #[must_use]
    pub fn keys(&self) -> &[JwkKey] {
        &self.keys
    }

    /// `getKtyFromAlg(alg)` — the key type an `alg` value implies. [jose `jwks/local.js`]
    ///
    /// `None` is `jose`'s `JOSENotSupported`, which is *not* a "no matching key": it never
    /// triggers a JWKS reload.
    #[must_use]
    pub fn key_type_for_algorithm(algorithm: &str) -> Option<&'static str> {
        match algorithm.get(..2) {
            Some("RS" | "PS") => Some("RSA"),
            Some("ES") => Some("EC"),
            Some("Ed") => Some("OKP"),
            _ => None,
        }
    }

    /// `LocalJWKSet#getKey` — the single candidate for `(alg, kid)`.
    ///
    /// # Errors
    /// [`SelectionError::UnsupportedAlgorithm`], [`SelectionError::NoMatchingKey`] or
    /// [`SelectionError::MultipleMatchingKeys`].
    pub fn select(&self, algorithm: &str, kid: Option<&str>) -> Result<&JwkKey, SelectionError> {
        let key_type =
            Self::key_type_for_algorithm(algorithm).ok_or(SelectionError::UnsupportedAlgorithm)?;
        let mut candidates = self.keys.iter().filter(|key| {
            key.kty() == Some(key_type)
                // `typeof kid === "string"` guards the kid filter, so a header without a `kid`
                // matches on the other criteria alone.
                && kid.is_none_or(|wanted| key.kid() == Some(wanted))
                // Applied only when the key declares its own `alg`.
                && key.alg().is_none_or(|declared| declared == algorithm)
                // Applied only when the key declares `use`.
                && key.string("use").is_none_or(|usage| usage == "sig")
                && key.allows_verify()
                && key.curve_matches(algorithm)
        });
        let first = candidates.next().ok_or(SelectionError::NoMatchingKey)?;
        if candidates.next().is_some() {
            return Err(SelectionError::MultipleMatchingKeys);
        }
        Ok(first)
    }
}

/// Why a key set produced no usable key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelectionError {
    /// `JOSENotSupported` — the `alg` prefix names no JWS key family.
    UnsupportedAlgorithm,
    /// `JWKSNoMatchingKey` — the only outcome that may trigger a rotation refetch.
    NoMatchingKey,
    /// `JWKSMultipleMatchingKeys` — an ambiguous set is refused, never disambiguated.
    MultipleMatchingKeys,
}

impl SelectionError {
    /// The public message, which never contains token or key material.
    #[must_use]
    pub const fn message(self) -> &'static str {
        match self {
            Self::UnsupportedAlgorithm => UNSUPPORTED_ALGORITHM_MESSAGE,
            Self::NoMatchingKey => NO_MATCHING_KEY_MESSAGE,
            Self::MultipleMatchingKeys => MULTIPLE_MATCHING_KEYS_MESSAGE,
        }
    }
}

impl fmt::Display for SelectionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message())
    }
}

impl From<SelectionError> for AppError {
    fn from(error: SelectionError) -> Self {
        Self::Unauthorized(error.message().to_owned())
    }
}

/// A key selected for one token, paired with the algorithm it may be used with.
///
/// The algorithm travels with the key so a verifier cannot be told to use the *token's* declared
/// algorithm with a key that was chosen for a different one.
#[derive(Clone, Debug)]
pub struct VerificationKey {
    algorithm: Algorithm,
    key: DecodingKey,
}

impl VerificationKey {
    /// The algorithm the key set authorises for this key.
    #[must_use]
    pub const fn algorithm(&self) -> Algorithm {
        self.algorithm
    }

    /// The decoding key.
    #[must_use]
    pub const fn key(&self) -> &DecodingKey {
        &self.key
    }
}

// ---------------------------------------------------------------------------
// Ports
// ---------------------------------------------------------------------------

/// Resolves the verification key for one JWS header.
///
/// The `alg`/`kid` pair is passed as the *raw header strings* because `jose` filters on them as
/// strings; mapping to a typed algorithm first would silently widen or narrow the match.
pub trait JwksProvider: Send + Sync + fmt::Debug {
    /// The single key authorised to verify a token with this header.
    ///
    /// # Errors
    /// Returns [`AppError`] when the set cannot be fetched, is malformed, names no matching key,
    /// or is ambiguous.
    fn resolve<'a>(
        &'a self,
        algorithm: &'a str,
        kid: Option<&'a str>,
    ) -> BoxFuture<'a, Result<VerificationKey, AppError>>;
}

/// Fetches a key set from wherever it lives.
///
/// Splitting transport from caching lets the rotation matrix be tested against a scripted source
/// with no network and no sleeping.
pub trait JwksSource: Send + Sync + fmt::Debug {
    /// Retrieve the current key set.
    ///
    /// # Errors
    /// Returns [`AppError`] when the endpoint is unreachable, answers with a non-200, or returns
    /// something that is not a key set.
    fn fetch(&self) -> BoxFuture<'_, Result<JwkDocument, AppError>>;
}

// ---------------------------------------------------------------------------
// HTTP source
// ---------------------------------------------------------------------------

/// The real Cloudflare Access endpoint. [lib/identity.js:44]
#[derive(Debug)]
pub struct HttpJwksSource {
    client: reqwest::Client,
    url: String,
}

impl HttpJwksSource {
    /// A source reading `url`, with `jose`'s 5-second timeout.
    ///
    /// # Errors
    /// Returns [`AppError::Internal`] when the HTTP client cannot be built.
    pub fn new(url: impl Into<String>) -> Result<Self, AppError> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_millis(FETCH_TIMEOUT_MS))
            .build()
            .map_err(|_| AppError::Internal)?;
        Ok(Self {
            client,
            url: url.into(),
        })
    }

    /// The endpoint this source reads.
    #[must_use]
    pub fn url(&self) -> &str {
        &self.url
    }
}

impl JwksSource for HttpJwksSource {
    fn fetch(&self) -> BoxFuture<'_, Result<JwkDocument, AppError>> {
        Box::pin(async move {
            let unavailable = || AppError::Unavailable(JWKS_UNAVAILABLE_MESSAGE.to_owned());
            let response = self
                .client
                .get(&self.url)
                .send()
                .await
                .map_err(|_| unavailable())?;
            // `jose`'s fetch_jwks rejects any non-200 before parsing.
            if response.status() != reqwest::StatusCode::OK {
                return Err(unavailable());
            }
            let body = response.bytes().await.map_err(|_| unavailable())?;
            if body.len() > MAX_JWKS_BYTES {
                return Err(unavailable());
            }
            JwkDocument::from_slice(&body)
        })
    }
}

// ---------------------------------------------------------------------------
// Caching provider
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct CacheState {
    document: Option<Arc<JwkDocument>>,
    fetched_at_millis: Option<i64>,
}

/// `createRemoteJWKSet` — a cached, self-rotating key set.
///
/// # Concurrency
///
/// `jose` dedupes concurrent reloads through a shared pending promise. This holds a
/// [`tokio::sync::Mutex`] across the fetch instead, so a second caller waits and then observes
/// the now-fresh cache. The observable outcome — one fetch, both callers served — is the same;
/// the timing is not, which is why the state is behind an async mutex rather than a blocking one.
#[derive(Debug)]
pub struct CachingJwks {
    source: Arc<dyn JwksSource>,
    clock: Arc<dyn Clock>,
    cache_max_age_ms: i64,
    cooldown_ms: i64,
    state: Mutex<CacheState>,
}

impl CachingJwks {
    /// A provider over `source` with `jose`'s default cache windows and the system clock.
    #[must_use]
    pub fn new(source: Arc<dyn JwksSource>) -> Self {
        Self::with_clock(source, Arc::new(SystemClock))
    }

    /// A provider over `source` whose freshness and cooldown windows follow `clock`.
    #[must_use]
    pub fn with_clock(source: Arc<dyn JwksSource>, clock: Arc<dyn Clock>) -> Self {
        Self {
            source,
            clock,
            cache_max_age_ms: CACHE_MAX_AGE_MS,
            cooldown_ms: COOLDOWN_MS,
            state: Mutex::new(CacheState::default()),
        }
    }

    /// Override the cache windows. Intended for tests that assert the boundaries directly.
    #[must_use]
    pub const fn with_windows(mut self, cache_max_age_ms: i64, cooldown_ms: i64) -> Self {
        self.cache_max_age_ms = cache_max_age_ms;
        self.cooldown_ms = cooldown_ms;
        self
    }

    /// The production provider for a configured team domain.
    ///
    /// # Errors
    /// Returns [`AppError::Internal`] when the HTTP client cannot be built.
    pub fn remote(url: impl Into<String>) -> Result<Self, AppError> {
        Ok(Self::new(Arc::new(HttpJwksSource::new(url)?)))
    }

    /// `fresh()` — `Date.now() < timestamp + cacheMaxAge`.
    fn fresh(&self, state: &CacheState, now: i64) -> bool {
        state
            .fetched_at_millis
            .is_some_and(|fetched| now < fetched.saturating_add(self.cache_max_age_ms))
    }

    /// `coolingDown()` — `Date.now() < timestamp + cooldownDuration`.
    fn cooling_down(&self, state: &CacheState, now: i64) -> bool {
        state
            .fetched_at_millis
            .is_some_and(|fetched| now < fetched.saturating_add(self.cooldown_ms))
    }

    /// `reload()` — replace the cached set and stamp it.
    ///
    /// A failed fetch leaves the previous set and its timestamp untouched, matching `jose`, which
    /// clears only the pending promise and rethrows.
    async fn reload(&self, state: &mut CacheState) -> Result<(), AppError> {
        let document = self.source.fetch().await?;
        state.document = Some(Arc::new(document));
        state.fetched_at_millis = Some(self.clock.now_unix_millis());
        Ok(())
    }
}

impl JwksProvider for CachingJwks {
    fn resolve<'a>(
        &'a self,
        algorithm: &'a str,
        kid: Option<&'a str>,
    ) -> BoxFuture<'a, Result<VerificationKey, AppError>> {
        Box::pin(async move {
            let parsed = algorithm
                .parse::<Algorithm>()
                .map_err(|_| AppError::Unauthorized(UNSUPPORTED_ALGORITHM_MESSAGE.to_owned()))?;
            let unavailable = || AppError::Unavailable(JWKS_UNAVAILABLE_MESSAGE.to_owned());

            let mut state = self.state.lock().await;
            if state.document.is_none() || !self.fresh(&state, self.clock.now_unix_millis()) {
                self.reload(&mut state).await?;
            }

            let cached = {
                let document = state.document.as_ref().ok_or_else(unavailable)?;
                match document.select(algorithm, kid) {
                    Ok(key) => Some(key.decoding_key(parsed)?),
                    // Only `JWKSNoMatchingKey` rotates; unsupported or ambiguous never does.
                    Err(SelectionError::NoMatchingKey)
                        if !self.cooling_down(&state, self.clock.now_unix_millis()) =>
                    {
                        None
                    }
                    Err(error) => return Err(error.into()),
                }
            };

            let key = match cached {
                Some(key) => key,
                None => {
                    self.reload(&mut state).await?;
                    let document = state.document.as_ref().ok_or_else(unavailable)?;
                    document.select(algorithm, kid)?.decoding_key(parsed)?
                }
            };

            Ok(VerificationKey {
                algorithm: parsed,
                key,
            })
        })
    }
}

/// A provider backed by one immutable key set: no fetching, no rotation.
///
/// This is the shape an offline deployment or a focused test needs, and it is the only provider
/// that can never make a network call.
#[derive(Debug)]
pub struct StaticJwks {
    document: JwkDocument,
}

impl StaticJwks {
    /// A provider serving `document`.
    #[must_use]
    pub const fn new(document: JwkDocument) -> Self {
        Self { document }
    }
}

impl JwksProvider for StaticJwks {
    fn resolve<'a>(
        &'a self,
        algorithm: &'a str,
        kid: Option<&'a str>,
    ) -> BoxFuture<'a, Result<VerificationKey, AppError>> {
        Box::pin(async move {
            let parsed = algorithm
                .parse::<Algorithm>()
                .map_err(|_| AppError::Unauthorized(UNSUPPORTED_ALGORITHM_MESSAGE.to_owned()))?;
            let key = self.document.select(algorithm, kid)?.decoding_key(parsed)?;
            Ok(VerificationKey {
                algorithm: parsed,
                key,
            })
        })
    }
}
