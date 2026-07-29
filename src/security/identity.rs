//! Owned by U05 (sol) — viewer identity and Cloudflare JWT verification.
//!
//! Authority: `lib/identity.js`. The claim-validation semantics it delegates to come from
//! `jose@5.10.0` (`node_modules/jose/dist/node/esm/lib/jwt_claims_set.js`), which is why the
//! boundary conditions below are quoted from that file rather than paraphrased.
//!
//! # Startup modes — `ACCESS_IDENTITY_MODE` [lib/identity.js:49-55]
//!
//! | Mode | Selected when | Identity source |
//! |---|---|---|
//! | `jwt` | `CF_ACCESS_TEAM_DOMAIN` **and** `CF_ACCESS_AUD` are both set | a verified `CF_Authorization` JWT |
//! | `header-trust` | neither, but `TRUST_ACCESS_HEADERS=1` | the *unverified* `Cf-Access-Authenticated-User-Email` header |
//! | `disabled` | neither | nothing — every request is anonymous |
//!
//! `header-trust` never reads a JWT and `jwt` never reads the email header: a spoofed email
//! header cannot bypass verification, and a JWT cannot smuggle an identity into a dev box.
//! [`assert_ready`] is the startup gate: `REQUIRE_ACCESS_JWT=1` without a usable JWT
//! configuration refuses to boot, and `header-trust` on a non-loopback bind refuses to boot
//! unless `HEADER_TRUST_ALLOW_INSECURE=1` acknowledges it. [lib/identity.js:56-76]
//!
//! # Token source and precedence [lib/identity.js:117-119]
//!
//! `Cf-Access-Jwt-Assertion` is preferred; the `CF_Authorization` cookie is the fallback. The
//! header wins *even when it is unusable* — Node evaluates `headerToken || readAccessCookie(req)`,
//! so a present-but-garbage header is used and fails, and the cookie is never consulted. That is
//! load-bearing: `access_retry.rs` exists precisely because Cloudflare sometimes sends the cookie
//! without the header.
//!
//! # Claim validation, with a 60-second leeway [lib/identity.js:22,122-126]
//!
//! `clockTolerance` defaults to 60 seconds (`Number(ACCESS_CLOCK_TOLERANCE_S) || 60`, so `0` and
//! unparsable values both select 60). `jose` applies it asymmetrically, and the asymmetry is
//! exact, not approximate:
//!
//! | Claim | Rejected when | Boundary |
//! |---|---|---|
//! | `nbf` | `nbf > now + tolerance` | `nbf == now + 60` is **accepted** |
//! | `exp` | `exp <= now - tolerance` | `exp == now - 60` is **rejected** |
//!
//! `iss` and `aud` are both *required* claims here, because `jose` adds any option it was given
//! to its presence check. A missing `iss`, a missing `aud`, or a mismatch of either yields no
//! identity.
//!
//! # Fail-closed
//!
//! Every verification failure — no token, malformed compact form, unknown `kid`, bad signature,
//! wrong `iss`/`aud`, expired or not-yet-valid beyond the leeway, malformed cookie, unusable
//! JWKS — collapses to an anonymous [`Viewer`]. Node's `catch { return {email:null, …} }`
//! ([lib/identity.js:128-130]) makes that the contract, and it is what
//! `human-concealment.invariant3` depends on.
//!
//! # Secret containment
//!
//! [`AccessToken`] redacts itself in `Debug`/`Display`. No token, claim set, or signature is ever
//! placed in an [`AppError`] or a log line; failures are reported as the anonymous viewer.

use std::{fmt, sync::Arc};

use axum::http::HeaderMap;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD as BASE64URL};
use serde_json::{Map, Value};

use crate::{
    config::{AccessConfig, AccessIdentityMode, AppConfig, Clock, SystemClock},
    error::AppError,
    model::{EmailAddress, OrgId, Viewer},
    ports::{BoxFuture, ViewerIdentity},
    security::{
        auth::ADMIN_ORG,
        jwks::{JwksProvider, StaticJwks},
    },
};

/// The header Cloudflare Access sets with the signed assertion. [lib/identity.js:117]
pub const ACCESS_JWT_HEADER: &str = "cf-access-jwt-assertion";
/// The header Cloudflare Access sets with the authenticated email. [lib/identity.js:132]
pub const ACCESS_EMAIL_HEADER: &str = "cf-access-authenticated-user-email";
/// The session cookie carrying the same assertion. [lib/identity.js:103]
pub const ACCESS_COOKIE_NAME: &str = "CF_Authorization";

/// A Cloudflare Access assertion.
///
/// It is a bearer credential for the whole session, so it redacts itself exactly like a
/// publisher key.
#[derive(Clone, PartialEq, Eq)]
pub struct AccessToken(String);

impl AccessToken {
    /// Wrap an extracted token.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Borrow the compact serialization. Callers must not log the result.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for AccessToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("AccessToken(<redacted>)")
    }
}

impl fmt::Display for AccessToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("<redacted>")
    }
}

// ---------------------------------------------------------------------------
// Header and cookie extraction
// ---------------------------------------------------------------------------

/// The first value of a header, or `""` when absent or not valid UTF-8.
///
/// Node normalises a repeated header to a single joined string and `lib/identity.js` additionally
/// guards with `Array.isArray(h) ? h[0] : h`; taking the first value reproduces that guard.
fn first_header<'a>(headers: &'a HeaderMap, name: &str) -> &'a str {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
}

/// `readAccessCookie(req)` — the `CF_Authorization` value. [lib/identity.js:96-109]
///
/// Cookie values legitimately contain `=` (a JWT does not, but the parser must not assume it), so
/// the split is on the **first** separator only and the name is compared exactly after trimming.
/// A part with no `=`, a differently-named cookie, or an empty value is skipped rather than
/// terminating the scan, so `CF_Authorization=; CF_Authorization=real` still yields `real`.
#[must_use]
pub fn read_access_cookie(headers: &HeaderMap) -> Option<AccessToken> {
    for raw in headers.get_all(axum::http::header::COOKIE) {
        let Ok(value) = raw.to_str() else { continue };
        for part in value.split(';') {
            let Some(separator) = part.find('=') else {
                continue;
            };
            if part[..separator].trim() != ACCESS_COOKIE_NAME {
                continue;
            }
            let token = part[separator + 1..].trim();
            if !token.is_empty() {
                return Some(AccessToken::new(token));
            }
        }
    }
    None
}

/// The `Cf-Access-Jwt-Assertion` header, when present and non-empty. [lib/identity.js:117-118]
#[must_use]
pub fn access_assertion_header(headers: &HeaderMap) -> Option<AccessToken> {
    let value = first_header(headers, ACCESS_JWT_HEADER);
    (!value.is_empty()).then(|| AccessToken::new(value))
}

/// `headerToken || readAccessCookie(req)` — header first, cookie second. [lib/identity.js:119]
#[must_use]
pub fn access_token(headers: &HeaderMap) -> Option<AccessToken> {
    access_assertion_header(headers).or_else(|| read_access_cookie(headers))
}

// ---------------------------------------------------------------------------
// Org directory
// ---------------------------------------------------------------------------

/// The database-backed half of `orgForEmail`. [lib/orgs.js:58-67]
///
/// `ViewerIdentity::resolve` needs two org lookups that live in tables U09 owns. Declaring them
/// as their own narrow port keeps this unit testable without a database and keeps the frozen
/// `AdminService` surface out of the identity path.
pub trait OrgDirectory: Send + Sync + fmt::Debug {
    /// `orgForEmail(email)` — the org from the v21 `org_email_members` table.
    ///
    /// That table's primary key is `TEXT PRIMARY KEY COLLATE NOCASE`
    /// ([lib/migrations.js:419-421]), so the match is case-insensitive **in SQLite**, on top of
    /// the caller's own lowercasing. Implementations backed by anything other than that table
    /// must reproduce the case-insensitive match themselves.
    ///
    /// # Errors
    /// Returns [`AppError`] when the lookup fails; a missing row is `Ok(None)`.
    fn org_for_email<'a>(
        &'a self,
        email: &'a EmailAddress,
    ) -> BoxFuture<'a, Result<Option<OrgId>, AppError>>;

    /// `orgForDomain(domain)` — the org from the `org_domains` table. [lib/orgs.js:58-61]
    ///
    /// # Errors
    /// Returns [`AppError`] when the lookup fails; a missing row is `Ok(None)`.
    fn org_for_domain<'a>(
        &'a self,
        domain: &'a str,
    ) -> BoxFuture<'a, Result<Option<OrgId>, AppError>>;
}

/// A directory with no registered orgs: resolution falls through to the environment map and then
/// to the bare email domain, exactly as an empty registry does in Node.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoOrgDirectory;

impl OrgDirectory for NoOrgDirectory {
    fn org_for_email<'a>(
        &'a self,
        _email: &'a EmailAddress,
    ) -> BoxFuture<'a, Result<Option<OrgId>, AppError>> {
        Box::pin(async { Ok(None) })
    }

    fn org_for_domain<'a>(
        &'a self,
        _domain: &'a str,
    ) -> BoxFuture<'a, Result<Option<OrgId>, AppError>> {
        Box::pin(async { Ok(None) })
    }
}

// ---------------------------------------------------------------------------
// Email → org
// ---------------------------------------------------------------------------

/// `domainOf(email)` — the part after the **last** `@`, lowercased, or `""`.
/// [lib/identity.js:78-81]
///
/// `lastIndexOf` (not `indexOf`) is deliberate in Node and reproduced here: it is what makes a
/// local part containing `@` resolve to the real domain.
#[must_use]
pub fn domain_of(email: &str) -> String {
    email
        .rfind('@')
        .map_or_else(String::new, |at| email[at + 1..].to_lowercase())
}

/// `ADMIN_EMAILS.has(email.toLowerCase()) || ADMIN_DOMAINS.has(domain)` — [lib/identity.js:85]
///
/// Admin status is decided **before** any mapping is consulted, so no org mapping can demote a
/// configured admin.
#[must_use]
pub fn is_admin_email(access: &AccessConfig, email: &str) -> bool {
    let lowered = email.to_lowercase();
    access.admin_emails.contains(&lowered) || access.admin_email_domains.contains(&domain_of(email))
}

/// `explicit || orgForDomain(domain) || DOMAIN_ORG.get(domain) || domain` — [lib/identity.js:89]
///
/// JavaScript's `||` skips the empty string as well as `null`, so an org column that is present
/// but blank falls through to the next source. Filtering empties reproduces that.
#[must_use]
pub fn org_from_sources(
    explicit: Option<OrgId>,
    registered_domain: Option<OrgId>,
    configured_domain: Option<&str>,
    domain: &str,
) -> OrgId {
    let candidates = [
        explicit.map(|org| org.0),
        registered_domain.map(|org| org.0),
        configured_domain.map(str::to_owned),
        Some(domain.to_owned()),
    ];
    OrgId(
        candidates
            .into_iter()
            .flatten()
            .find(|candidate| !candidate.is_empty())
            .unwrap_or_default(),
    )
}

// ---------------------------------------------------------------------------
// JWT verification
// ---------------------------------------------------------------------------

/// A verification failure, always reported to the caller as an anonymous viewer.
///
/// The variants exist so tests can assert *why* a token was refused without any of them ever
/// being rendered into an HTTP response.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TokenRejection {
    /// Not three base64url segments, or a segment that is not valid base64url JSON.
    Malformed,
    /// The protected header has no usable `alg`.
    MissingAlgorithm,
    /// A `crit` header this verifier does not implement.
    UnsupportedCriticalHeader,
    /// The JWKS named no single usable key for this header.
    NoVerificationKey,
    /// The signature did not verify under the selected key.
    BadSignature,
    /// The claims set is not a JSON object.
    InvalidClaims,
    /// A claim `jose` requires (`iss` or `aud`) is absent.
    MissingClaim(&'static str),
    /// `iss` did not equal `https://<team-domain>`.
    WrongIssuer,
    /// `aud` did not contain `CF_ACCESS_AUD`.
    WrongAudience,
    /// `nbf > now + tolerance`.
    NotYetValid,
    /// `exp <= now - tolerance`.
    Expired,
    /// A temporal claim was present but not a number.
    InvalidTemporalClaim(&'static str),
    /// The JWKS endpoint could not be consulted.
    KeyUnavailable,
}

/// The verified claims a caller is allowed to see.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiedAccessToken {
    /// The `email` claim, trimmed and lowercased. [lib/identity.js:127]
    pub email: String,
}

/// Base64url-decode one compact segment.
fn decode_segment(segment: &str) -> Option<Vec<u8>> {
    BASE64URL.decode(segment).ok()
}

/// Decode a compact segment into a JSON object.
fn decode_json_object(segment: &str) -> Option<Map<String, Value>> {
    let bytes = decode_segment(segment)?;
    match serde_json::from_slice::<Value>(&bytes).ok()? {
        Value::Object(members) => Some(members),
        _ => None,
    }
}

/// `String(value || "")` for the shapes a JWT claim can actually hold.
///
/// `null`, `false`, `0`, `""` and `[]` are all falsy or stringify to `""` in JavaScript, so they
/// collapse to the empty string here too, which resolves to no identity.
///
/// # Two deliberate narrowings
///
/// * **An object claim.** `String({})` is `"[object Object]"` in JavaScript, and Node would carry
///   that through as a viewer email whose org resolves to `""`. Collapsing it to no identity is
///   strictly more fail-closed, and Cloudflare Access cannot issue a JWT whose `email` claim is
///   an object.
/// * **Float formatting.** `serde_json`'s rendering can differ from JavaScript's (`1.0` vs `1`).
///   No email claim is ever a float, and either rendering fails the subsequent org lookup
///   identically.
fn js_string_claim(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Bool(true)) => "true".to_owned(),
        Some(Value::Number(number)) if number.as_f64() != Some(0.0) => number.to_string(),
        _ => String::new(),
    }
}

/// `String.prototype.trim`'s character set, which is **not** Rust's `char::is_whitespace`.
///
/// ECMA-262 trims WhiteSpace ∪ LineTerminator: TAB, VT, FF, SP, NBSP, ZWNBSP (`U+FEFF`), every
/// `Zs`, LF, CR, LS, PS. Rust's White_Space property adds `U+0085` (NEL) and omits `U+FEFF`, so
/// both differences are corrected here.
#[must_use]
const fn is_js_whitespace(value: char) -> bool {
    matches!(value, '\u{feff}') || (value.is_whitespace() && !matches!(value, '\u{85}'))
}

/// `String.prototype.trim` — [lib/identity.js:127]
#[must_use]
fn js_trim(value: &str) -> &str {
    value.trim_matches(is_js_whitespace)
}

/// `checkAudiencePresence(payload.aud, [AUD])` — [jose `lib/jwt_claims_set.js`]
fn audience_matches(claim: Option<&Value>, expected: &str) -> bool {
    match claim {
        Some(Value::String(single)) => single == expected,
        Some(Value::Array(values)) => values.iter().any(|value| value.as_str() == Some(expected)),
        _ => false,
    }
}

/// `validateCrit` restricted to the one extension `jose` recognises by default.
///
/// `jose` accepts `crit: ["b64"]` only when `b64` is present, and `jwtVerify` then rejects the
/// token outright if `b64 === false`. Every other critical parameter is `JOSENotSupported`.
fn critical_headers_ok(header: &Map<String, Value>) -> bool {
    let Some(crit) = header.get("crit") else {
        return true;
    };
    let Some(parameters) = crit.as_array() else {
        return false;
    };
    if parameters.is_empty() {
        return false;
    }
    parameters.iter().all(|parameter| {
        parameter.as_str() == Some("b64") && header.get("b64") == Some(&Value::Bool(true))
    })
}

// ---------------------------------------------------------------------------
// Viewer identity
// ---------------------------------------------------------------------------

/// `createViewerResolver()` as the frozen [`ViewerIdentity`] port. [lib/identity.js:112-142]
#[derive(Clone, Debug)]
pub struct AccessViewerIdentity {
    config: Arc<AppConfig>,
    jwks: Arc<dyn JwksProvider>,
    directory: Arc<dyn OrgDirectory>,
    clock: Arc<dyn Clock>,
}

impl AccessViewerIdentity {
    /// A resolver for `config`, reading keys from `jwks` and orgs from `directory`.
    #[must_use]
    pub fn new(
        config: Arc<AppConfig>,
        jwks: Arc<dyn JwksProvider>,
        directory: Arc<dyn OrgDirectory>,
    ) -> Self {
        Self::with_clock(config, jwks, directory, Arc::new(SystemClock))
    }

    /// A resolver whose leeway arithmetic is evaluated against `clock`.
    #[must_use]
    pub fn with_clock(
        config: Arc<AppConfig>,
        jwks: Arc<dyn JwksProvider>,
        directory: Arc<dyn OrgDirectory>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            config,
            jwks,
            directory,
            clock,
        }
    }

    /// A resolver with no keys and no registry — the shape `disabled` and `header-trust` need.
    #[must_use]
    pub fn without_keys(config: Arc<AppConfig>) -> Self {
        Self::new(
            config,
            Arc::new(StaticJwks::new(
                crate::security::jwks::JwkDocument::default(),
            )),
            Arc::new(NoOrgDirectory),
        )
    }

    /// The active identity mode. [lib/identity.js:50-55]
    #[must_use]
    pub fn mode(&self) -> AccessIdentityMode {
        self.config.access.identity_mode()
    }

    /// `https://${TEAM_DOMAIN}` — the only accepted issuer. [lib/identity.js:123]
    #[must_use]
    pub fn expected_issuer(&self) -> String {
        format!("https://{}", self.config.access.team_domain)
    }

    /// Verify a Cloudflare Access assertion.
    ///
    /// # Errors
    /// Returns the [`TokenRejection`] describing the first failed check. Callers on the request
    /// path must discard it and return an anonymous viewer.
    pub async fn verify(&self, token: &AccessToken) -> Result<VerifiedAccessToken, TokenRejection> {
        let access = &self.config.access;

        // --- compact serialization -----------------------------------------
        let mut segments = token.expose().split('.');
        let (Some(protected), Some(payload), Some(signature), None) = (
            segments.next(),
            segments.next(),
            segments.next(),
            segments.next(),
        ) else {
            return Err(TokenRejection::Malformed);
        };

        let header = decode_json_object(protected).ok_or(TokenRejection::Malformed)?;
        if !critical_headers_ok(&header) {
            return Err(TokenRejection::UnsupportedCriticalHeader);
        }
        let algorithm = header
            .get("alg")
            .and_then(Value::as_str)
            .filter(|alg| !alg.is_empty())
            .ok_or(TokenRejection::MissingAlgorithm)?;
        let kid = header.get("kid").and_then(Value::as_str);

        // --- signature -----------------------------------------------------
        // The key set decides which algorithm the chosen key may be used with, so a token
        // cannot nominate an algorithm the key was not published for.
        let key = self
            .jwks
            .resolve(algorithm, kid)
            .await
            .map_err(|error| match error {
                AppError::Unavailable(_) | AppError::Internal => TokenRejection::KeyUnavailable,
                _ => TokenRejection::NoVerificationKey,
            })?;
        let message = format!("{protected}.{payload}");
        let verified =
            jsonwebtoken::crypto::verify(signature, message.as_bytes(), key.key(), key.algorithm())
                .unwrap_or(false);
        if !verified {
            return Err(TokenRejection::BadSignature);
        }

        // --- claims --------------------------------------------------------
        let claims = decode_json_object(payload).ok_or(TokenRejection::InvalidClaims)?;
        // `jose` adds `aud` and `iss` to its presence check because both options were supplied.
        if !claims.contains_key("aud") {
            return Err(TokenRejection::MissingClaim("aud"));
        }
        if !claims.contains_key("iss") {
            return Err(TokenRejection::MissingClaim("iss"));
        }
        if claims.get("iss").and_then(Value::as_str) != Some(self.expected_issuer().as_str()) {
            return Err(TokenRejection::WrongIssuer);
        }
        if !audience_matches(claims.get("aud"), &access.aud) {
            return Err(TokenRejection::WrongAudience);
        }

        let tolerance = i64::try_from(access.clock_tolerance_seconds).unwrap_or(i64::MAX);
        let now = self.clock.now_unix_seconds();

        // `(payload.iat !== undefined) && typeof payload.iat !== "number"` is checked before the
        // temporal claims, so a string `iat` fails even though no age limit is configured.
        if let Some(iat) = claims.get("iat")
            && iat.as_f64().is_none()
        {
            return Err(TokenRejection::InvalidTemporalClaim("iat"));
        }
        if let Some(nbf) = claims.get("nbf") {
            let seconds = nbf
                .as_f64()
                .ok_or(TokenRejection::InvalidTemporalClaim("nbf"))?;
            // `nbf > now + tolerance` — equality is inside the window.
            if seconds > now.saturating_add(tolerance) as f64 {
                return Err(TokenRejection::NotYetValid);
            }
        }
        if let Some(exp) = claims.get("exp") {
            let seconds = exp
                .as_f64()
                .ok_or(TokenRejection::InvalidTemporalClaim("exp"))?;
            // `exp <= now - tolerance` — equality is *outside* the window.
            if seconds <= now.saturating_sub(tolerance) as f64 {
                return Err(TokenRejection::Expired);
            }
        }

        Ok(VerifiedAccessToken {
            email: js_trim(&js_string_claim(claims.get("email"))).to_lowercase(),
        })
    }

    /// `orgForEmail(email)` — the org and admin flag for an authenticated address.
    ///
    /// # Errors
    /// Returns [`AppError`] only when a directory lookup itself fails.
    pub async fn org_for_email(&self, email: &str) -> Result<(OrgId, bool), AppError> {
        let access = &self.config.access;
        let domain = domain_of(email);
        if is_admin_email(access, email) {
            return Ok((OrgId(ADMIN_ORG.to_owned()), true));
        }
        let normalized = EmailAddress(email.trim().to_lowercase());
        let explicit = self.directory.org_for_email(&normalized).await?;
        let registered = self.directory.org_for_domain(&domain).await?;
        let configured = access.domain_orgs.get(&domain).map(String::as_str);
        Ok((
            org_from_sources(explicit, registered, configured, &domain),
            false,
        ))
    }

    /// The email this request presents, before any org resolution.
    ///
    /// `Ok(None)` is "no identity"; it is never an error, because Node's resolver has no failure
    /// mode other than "anonymous".
    async fn presented_email(&self, headers: &HeaderMap) -> Option<String> {
        match self.mode() {
            AccessIdentityMode::Jwt => {
                let token = access_token(headers)?;
                self.verify(&token).await.ok().map(|claims| claims.email)
            }
            AccessIdentityMode::HeaderTrust => Some(
                first_header(headers, ACCESS_EMAIL_HEADER)
                    .trim()
                    .to_lowercase(),
            ),
            AccessIdentityMode::Disabled => None,
        }
    }
}

impl ViewerIdentity for AccessViewerIdentity {
    fn resolve<'a>(&'a self, headers: &'a HeaderMap) -> BoxFuture<'a, Result<Viewer, AppError>> {
        Box::pin(async move {
            let Some(email) = self
                .presented_email(headers)
                .await
                .filter(|e| !e.is_empty())
            else {
                return Ok(Viewer::default());
            };
            let (org, is_admin) = self.org_for_email(&email).await?;
            Ok(Viewer {
                email: Some(EmailAddress(email)),
                org: Some(org),
                is_admin,
            })
        })
    }
}

/// `assertReady()` — the startup gate for the configured identity mode. [lib/identity.js:56-76]
///
/// The checks themselves live in `AppConfig::validate_startup`, which U02 ported together with
/// the environment parsing; this is the security layer's named entry point so a bootstrap does
/// not have to know that. Re-parsing the environment here would risk the two drifting apart.
///
/// # Errors
/// Returns [`AppError::Validation`] carrying Node's verbatim refusal message when
/// `REQUIRE_ACCESS_JWT=1` is set without a usable JWT configuration, or when `header-trust` is
/// selected on a non-loopback bind without `HEADER_TRUST_ALLOW_INSECURE=1`.
pub fn assert_ready(config: &AppConfig) -> Result<(), AppError> {
    config.validate_startup()
}
