//! Owned by U02 (terra) — typed environment, limits, clocks, IDs, and random sources.
//!
//! Every default in this module is transcribed from the Node reference, which is the
//! behavioural oracle for the rebuild. Each constant cites the `file:line` it came from.
//!
//! Parsing policy (fail-closed):
//!
//! * An **absent** variable, or one whose value is empty after trimming, uses the Node
//!   default. Node reaches the same result because `Number("")` is `0` (not `> 0`) and
//!   `"" || fallback` yields the fallback, so this is behaviour-preserving.
//! * A **present, non-empty** variable that Node would silently discard (`NaN`, negative,
//!   zero, unparsable byte size, malformed URL) is rejected with [`AppError::Validation`]
//!   instead of silently falling back. This is the one deliberate hardening over Node:
//!   it never changes the result for an input Node accepts, and no conformance case
//!   supplies an input Node would discard.
//! * Nothing in this module panics, and the crate forbids `unsafe`.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    path::{Path, PathBuf},
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
use time::OffsetDateTime;

use crate::{
    error::AppError,
    model::{ArtifactId, ClientId, FeedbackId, OrgId, ShareToken, Timestamp, WebhookId},
};

// ---------------------------------------------------------------------------
// Environment source
// ---------------------------------------------------------------------------

/// Read-only environment lookup.
///
/// Configuration is parsed through this seam so native tests never mutate the process
/// environment. `std::env::set_var` is `unsafe` in edition 2024 and would race across
/// cargo's parallel test threads; a map-backed source is both safe and deterministic.
pub trait EnvSource {
    /// Return the raw value for `key`, exactly as the process would see it.
    fn get(&self, key: &str) -> Option<String>;
}

/// Production source backed by the real process environment.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemEnv;

impl EnvSource for SystemEnv {
    fn get(&self, key: &str) -> Option<String> {
        std::env::var(key).ok()
    }
}

/// Deterministic in-memory environment used by tests and by [`AppConfig::defaults`].
#[derive(Clone, Debug, Default)]
pub struct MapEnv {
    entries: BTreeMap<String, String>,
}

impl MapEnv {
    /// An environment with no variables set at all.
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    /// Builder-style setter.
    #[must_use]
    pub fn with(mut self, key: &str, value: &str) -> Self {
        self.entries.insert(key.to_owned(), value.to_owned());
        self
    }
}

impl<K, V> FromIterator<(K, V)> for MapEnv
where
    K: Into<String>,
    V: Into<String>,
{
    fn from_iter<I: IntoIterator<Item = (K, V)>>(iter: I) -> Self {
        Self {
            entries: iter
                .into_iter()
                .map(|(key, value)| (key.into(), value.into()))
                .collect(),
        }
    }
}

impl EnvSource for MapEnv {
    fn get(&self, key: &str) -> Option<String> {
        self.entries.get(key).cloned()
    }
}

// ---------------------------------------------------------------------------
// Secrets
// ---------------------------------------------------------------------------

/// A configured secret whose `Debug`/`Display` output is always redacted.
///
/// Blueprint A8 requires that error and startup logs never contain bearer keys or
/// webhook credentials; wrapping them here makes leaking one require an explicit
/// [`Secret::expose`] call.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    /// Wrap a secret value.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Borrow the secret value. Callers must not log the result.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Secret(<redacted>)")
    }
}

impl fmt::Display for Secret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("<redacted>")
    }
}

// ---------------------------------------------------------------------------
// Node defaults
// ---------------------------------------------------------------------------

/// JavaScript `Number.MAX_SAFE_INTEGER`; `positiveInteger` rejects anything above it.
/// [lib/config.js:5]
pub const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

/// `Number(process.env.PORT || 3480)` — [server.js:27]
pub const DEFAULT_PORT: u16 = 3480;
/// `process.env.LISTEN_HOST || "0.0.0.0"` — [server.js:190], [lib/identity.js:66]
pub const DEFAULT_LISTEN_HOST: &str = "0.0.0.0";
/// `process.env.PUBLIC_BASE_URL || "http://localhost:3480"` — [server.js:28], [lib/app.js:67]
pub const DEFAULT_PUBLIC_BASE_URL: &str = "http://localhost:3480";
/// `process.env.DATA_DIR || "/data"` — [lib/db.js:8], [lib/thumbnails.js:66]
pub const DEFAULT_DATA_DIR: &str = "/data";

/// `nonEmptyString(process.env.APP_NAME, "Artifact Index")` — [lib/config.js:19]
pub const DEFAULT_APP_NAME: &str = "Artifact Index";
/// `nonEmptyString(process.env.APP_BRAND, "A")` — [lib/config.js:20]
pub const DEFAULT_APP_BRAND: &str = "A";

/// `positiveInteger(process.env.FEEDBACK_MAX_BODY, 4000)` — [lib/config.js:21]
pub const DEFAULT_FEEDBACK_MAX_BODY: u64 = 4000;
/// `positiveInteger(process.env.MAX_HISTORY, 20)` — [lib/config.js:23]
pub const DEFAULT_MAX_HISTORY: u64 = 20;
/// `positiveInteger(process.env.MAX_ARTIFACT_BYTES, 2 * 1024 * 1024)` — [lib/config.js:24]
pub const DEFAULT_MAX_ARTIFACT_BYTES: u64 = 2 * 1024 * 1024;
/// `positiveInteger(process.env.MAX_BUNDLE_BYTES, 8 * 1024 * 1024)` — [lib/config.js:25]
pub const DEFAULT_MAX_BUNDLE_BYTES: u64 = 8 * 1024 * 1024;
/// `positiveInteger(process.env.MAX_BUNDLE_FILES, 100)` — [lib/config.js:26]
pub const DEFAULT_MAX_BUNDLE_FILES: u64 = 100;

/// `express.json({ limit: limits.keyJson || "64kb" })` — [lib/app.js:261]
pub const DEFAULT_KEY_JSON_LIMIT: u64 = 64 * 1024;
/// `express.json({ limit: limits.reactionJson || "8kb" })` — [lib/app.js:545]
pub const DEFAULT_REACTION_JSON_LIMIT: u64 = 8 * 1024;
/// `express.json({ limit: limits.feedbackJson || "16kb" })` — [lib/app.js:563]
pub const DEFAULT_FEEDBACK_JSON_LIMIT: u64 = 16 * 1024;
/// `express.json({ limit: limits.categoryJson || "8kb" })` — [lib/app.js:634]
pub const DEFAULT_CATEGORY_JSON_LIMIT: u64 = 8 * 1024;
/// The `|| "8mb"` fallback `/mcp` uses when the composition root supplies no limit.
/// `server.js` always supplies `MCP_JSON_LIMIT`, so this is a defence-in-depth floor.
/// [lib/app.js:152]
pub const DEFAULT_MCP_JSON_FALLBACK_LIMIT: u64 = 8 * 1024 * 1024;

/// `Number(process.env.ACCESS_CLOCK_TOLERANCE_S) || 60` — [lib/identity.js:22]
pub const DEFAULT_ACCESS_CLOCK_TOLERANCE_SECONDS: u64 = 60;
/// OAuth access-token clock tolerance. Service credentials use a narrower window than browser
/// sessions because token acquisition is fully automated.
pub const DEFAULT_OAUTH_CLOCK_TOLERANCE_SECONDS: u64 = 30;
/// Maximum accepted OAuth access-token lifetime, measured from `iat` to `exp`.
pub const DEFAULT_OAUTH_MAX_TOKEN_LIFETIME_SECONDS: u64 = 3_600;

/// `const DEFAULT_TIMEOUT_MS = 8000` — [lib/preview.js:6]
pub const DEFAULT_PREVIEW_TIMEOUT_MS: u64 = 8000;
/// `const DEFAULT_VIEWPORT = "1200x630"` — [lib/preview.js:7]
pub const DEFAULT_PREVIEW_VIEWPORT_WIDTH: u64 = 1200;
/// `const DEFAULT_VIEWPORT = "1200x630"` — [lib/preview.js:7]
pub const DEFAULT_PREVIEW_VIEWPORT_HEIGHT: u64 = 630;
/// `const DEFAULT_CACHE_ENTRIES = 32` — [lib/preview.js:8]
pub const DEFAULT_PREVIEW_CACHE_ENTRIES: u64 = 32;
/// `export const DEFAULT_MAX_PNG_BYTES = 7_500_000` — [lib/thumbnails.js:17]
pub const DEFAULT_PREVIEW_MAX_PNG_BYTES: u64 = 7_500_000;

/// Documented placeholder secrets that `seedKeysFromEnv` refuses to seed. [lib/db.js:38]
pub const PLACEHOLDER_KEY_SECRETS: [&str; 2] = ["CHANGE_ME", "REPLACE_WITH_LONG_RANDOM_SECRET"];

/// Back-compat org for a two-part `ARTIFACT_API_KEYS` entry — [lib/db.js:46]
pub const DEFAULT_SEED_KEY_ORG: &str = "default";

/// `mcpJsonLimitFor(maxBundleBytes)` — [lib/config.js:13-17]
///
/// A one-byte control character expands to a six-byte `\u00XX` JSON escape, plus a fixed
/// 256 KiB envelope for JSON-RPC metadata, file names, titles, and descriptions.
#[must_use]
pub const fn mcp_json_limit_for(max_bundle_bytes: u64) -> u64 {
    max_bundle_bytes
        .saturating_mul(6)
        .saturating_add(256 * 1024)
}

// ---------------------------------------------------------------------------
// Identifier alphabets and lengths
// ---------------------------------------------------------------------------

/// Artifact id alphabet — `customAlphabet("0123456789abcdefghijkmnpqrstuvwxyz", 12)`.
/// Note the deliberately absent `l` and `o`. [lib/store.js:30]
pub const ARTIFACT_ID_ALPHABET: &str = "0123456789abcdefghijkmnpqrstuvwxyz";
/// Artifact id length — [lib/store.js:30]
pub const ARTIFACT_ID_LENGTH: usize = 12;

/// Public share token alphabet (nanoid's URL-safe alphabet) — [lib/shares.js:6]
pub const SHARE_TOKEN_ALPHABET: &str =
    "0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz_-";
/// Public share token length — [lib/shares.js:6]
pub const SHARE_TOKEN_LENGTH: usize = 24;

/// Feedback id alphabet — `customAlphabet("0123456789abcdefghijklmnopqrstuvwxyz", 16)`.
/// Unlike artifact ids this one keeps `l` and `o`. [lib/feedback.js:10]
pub const FEEDBACK_ID_ALPHABET: &str = "0123456789abcdefghijklmnopqrstuvwxyz";
/// Feedback id length — [lib/feedback.js:10]
pub const FEEDBACK_ID_LENGTH: usize = 16;

/// Webhook id alphabet — shares the artifact alphabet. [lib/webhooks.js:11]
pub const WEBHOOK_ID_ALPHABET: &str = ARTIFACT_ID_ALPHABET;
/// Webhook id length — [lib/webhooks.js:11]
pub const WEBHOOK_ID_LENGTH: usize = 12;

/// `const ARTIFACT_ID = /^[0-9a-z]{6,24}$/` — the real validator every generated artifact
/// id must satisfy before a thumbnail path is derived from it. [lib/thumbnails.js:19]
#[must_use]
pub fn is_valid_artifact_id(value: &str) -> bool {
    (6..=24).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || byte.is_ascii_lowercase())
}

/// `const RESERVED = new Set([...])` — ids that can never address an artifact.
/// The empty string is the seventh member in Node and is excluded here by construction.
/// [lib/store.js:29]
pub const RESERVED_ARTIFACT_IDS: [&str; 7] = [
    "mcp",
    "health",
    "settings",
    "raw",
    "s",
    "favicon.ico",
    "robots.txt",
];

// ---------------------------------------------------------------------------
// Parsing helpers
// ---------------------------------------------------------------------------

fn invalid(key: &str, raw: &str, expectation: &str) -> AppError {
    AppError::Validation(format!("{key} must be {expectation} (got \"{raw}\")"))
}

/// Fetch a variable, treating "absent" and "empty after trimming" identically.
///
/// Node reaches the same conclusion for both: `"" || fallback` and
/// `Number.isSafeInteger(Number("")) && 0 > 0` both select the fallback.
fn present(env: &dyn EnvSource, key: &str) -> Option<String> {
    env.get(key).filter(|value| !value.trim().is_empty())
}

/// Port of `nonEmptyString(value, fallback)` — [lib/config.js:8-11]
fn non_empty_string(env: &dyn EnvSource, key: &str, default: &str) -> String {
    present(env, key).map_or_else(|| default.to_owned(), |value| value.trim().to_owned())
}

/// Port of `value || fallback` for strings Node does **not** trim. [server.js:28], [lib/db.js:8]
fn raw_string(env: &dyn EnvSource, key: &str, default: &str) -> String {
    env.get(key)
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| default.to_owned())
}

/// Port of `positiveInteger(value, fallback)` — [lib/config.js:3-6]
///
/// Accepts exactly the decimal integers Node accepts; rejects (rather than silently
/// discarding) anything else that is actually present.
fn positive_integer(env: &dyn EnvSource, key: &str, default: u64) -> Result<u64, AppError> {
    let Some(raw) = present(env, key) else {
        return Ok(default);
    };
    let trimmed = raw.trim();
    let parsed: u64 = trimmed
        .parse()
        .map_err(|_| invalid(key, trimmed, "a positive integer"))?;
    if parsed == 0 || parsed > MAX_SAFE_INTEGER {
        return Err(invalid(key, trimmed, "a positive safe integer"));
    }
    Ok(parsed)
}

/// Port of `process.env.X === "1"` — [lib/identity.js:16,57,68]
fn flag(env: &dyn EnvSource, key: &str) -> bool {
    env.get(key).is_some_and(|value| value == "1")
}

fn enabled_unless_zero(env: &dyn EnvSource, key: &str) -> Result<bool, AppError> {
    match present(env, key).as_deref().map(str::trim) {
        None | Some("1") => Ok(true),
        Some("0") => Ok(false),
        Some(raw) => Err(invalid(key, raw, "\"0\" or \"1\"")),
    }
}

/// Port of the `bytes` package's `parse()` used by `express.json({ limit })`.
///
/// `bytes.parse` matches `/^((-|\+)?(\d+(?:\.\d+)?)) *(kb|mb|gb|tb|pb)$/i` and otherwise
/// falls back to `parseInt(val, 10)`, then floors `unit * value`. Only the accepting
/// branches are reproduced; unparsable values fail closed.
fn parse_byte_size(key: &str, raw: &str) -> Result<u64, AppError> {
    let trimmed = raw.trim();
    let lowered = trimmed.to_ascii_lowercase();
    let (number, multiplier) = split_byte_size(&lowered);
    let value: f64 = number
        .parse()
        .map_err(|_| invalid(key, trimmed, "a byte size such as \"8mb\" or \"1048576\""))?;
    if !value.is_finite() || value <= 0.0 {
        return Err(invalid(key, trimmed, "a positive byte size"));
    }
    #[expect(
        clippy::cast_precision_loss,
        reason = "byte-size multipliers are exact in f64 up to 1 PiB"
    )]
    let bytes = (value * multiplier as f64).floor();
    #[expect(
        clippy::cast_precision_loss,
        reason = "MAX_SAFE_INTEGER is exactly representable in f64"
    )]
    let ceiling = MAX_SAFE_INTEGER as f64;
    if bytes < 1.0 || bytes > ceiling {
        return Err(invalid(key, trimmed, "a positive byte size"));
    }
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "bytes is a floored, positive value bounded by MAX_SAFE_INTEGER"
    )]
    let bytes = bytes as u64;
    Ok(bytes)
}

fn split_byte_size(lowered: &str) -> (&str, u64) {
    const UNITS: [(&str, u64); 6] = [
        ("pb", 1024 * 1024 * 1024 * 1024 * 1024),
        ("tb", 1024 * 1024 * 1024 * 1024),
        ("gb", 1024 * 1024 * 1024),
        ("mb", 1024 * 1024),
        ("kb", 1024),
        ("b", 1),
    ];
    for (suffix, multiplier) in UNITS {
        if let Some(head) = lowered.strip_suffix(suffix) {
            return (head.trim_end(), multiplier);
        }
    }
    (lowered, 1)
}

// ---------------------------------------------------------------------------
// Limits
// ---------------------------------------------------------------------------

/// Upload/storage limits shared by the store and the MCP tools. [lib/config.js:21-27]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StorageLimits {
    /// `FEEDBACK_MAX_BODY` — maximum feedback body characters.
    pub feedback_max_body: u64,
    /// `MAX_HISTORY` — retained revisions per artifact.
    pub max_history: u64,
    /// `MAX_ARTIFACT_BYTES` — maximum single-file artifact size.
    pub max_artifact_bytes: u64,
    /// `MAX_BUNDLE_BYTES` — maximum total bundle size.
    pub max_bundle_bytes: u64,
    /// `MAX_BUNDLE_FILES` — maximum entries in a bundle.
    pub max_bundle_files: u64,
}

impl Default for StorageLimits {
    fn default() -> Self {
        Self {
            feedback_max_body: DEFAULT_FEEDBACK_MAX_BODY,
            max_history: DEFAULT_MAX_HISTORY,
            max_artifact_bytes: DEFAULT_MAX_ARTIFACT_BYTES,
            max_bundle_bytes: DEFAULT_MAX_BUNDLE_BYTES,
            max_bundle_files: DEFAULT_MAX_BUNDLE_FILES,
        }
    }
}

impl StorageLimits {
    fn from_source(env: &dyn EnvSource) -> Result<Self, AppError> {
        Ok(Self {
            feedback_max_body: positive_integer(
                env,
                "FEEDBACK_MAX_BODY",
                DEFAULT_FEEDBACK_MAX_BODY,
            )?,
            max_history: positive_integer(env, "MAX_HISTORY", DEFAULT_MAX_HISTORY)?,
            max_artifact_bytes: positive_integer(
                env,
                "MAX_ARTIFACT_BYTES",
                DEFAULT_MAX_ARTIFACT_BYTES,
            )?,
            max_bundle_bytes: positive_integer(env, "MAX_BUNDLE_BYTES", DEFAULT_MAX_BUNDLE_BYTES)?,
            max_bundle_files: positive_integer(env, "MAX_BUNDLE_FILES", DEFAULT_MAX_BUNDLE_FILES)?,
        })
    }
}

/// Per-route JSON body limits in bytes, mirroring every `express.json({ limit })` call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BodyLimits {
    /// `POST /mcp` — [lib/app.js:152], supplied by [server.js:186]
    pub mcp_json: u64,
    /// `/settings/keys`, `/settings/orgs/**` — [lib/app.js:261,302,322,341,362,373,380,392,409]
    pub key_json: u64,
    /// `POST /:id/react` — [lib/app.js:545]
    pub reaction_json: u64,
    /// `POST /:id/feedback` — [lib/app.js:563]
    pub feedback_json: u64,
    /// `POST /:id/{category,share,visibility,move,restore}` — [lib/app.js:634,643,669,679,708]
    pub category_json: u64,
}

impl Default for BodyLimits {
    fn default() -> Self {
        Self {
            mcp_json: mcp_json_limit_for(DEFAULT_MAX_BUNDLE_BYTES),
            key_json: DEFAULT_KEY_JSON_LIMIT,
            reaction_json: DEFAULT_REACTION_JSON_LIMIT,
            feedback_json: DEFAULT_FEEDBACK_JSON_LIMIT,
            category_json: DEFAULT_CATEGORY_JSON_LIMIT,
        }
    }
}

impl BodyLimits {
    fn from_source(env: &dyn EnvSource, max_bundle_bytes: u64) -> Result<Self, AppError> {
        // `MCP_JSON_LIMIT` is the only limit express receives as a raw string, so it is the
        // only one parsed with the `bytes` grammar. [lib/config.js:27], [lib/app.js:152]
        let mcp_json = match present(env, "MCP_JSON_LIMIT") {
            Some(raw) => parse_byte_size("MCP_JSON_LIMIT", &raw)?,
            None => mcp_json_limit_for(max_bundle_bytes),
        };
        Ok(Self {
            mcp_json,
            ..Self::default()
        })
    }
}

/// Optional HTML preview renderer configuration. [lib/preview.js:38-49], [lib/thumbnails.js:66-72]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreviewConfig {
    /// `PREVIEW_RENDERER_URL` normalized to the `render` endpoint, or `None` when disabled.
    pub renderer_endpoint: Option<String>,
    /// `PREVIEW_RENDER_TIMEOUT_MS`
    pub timeout_ms: u64,
    /// `PREVIEW_VIEWPORT` width component.
    pub viewport_width: u64,
    /// `PREVIEW_VIEWPORT` height component.
    pub viewport_height: u64,
    /// In-process render cache entries (not environment-driven). [lib/preview.js:8]
    pub cache_entries: u64,
    /// `PREVIEW_MAX_PNG_BYTES`
    pub max_png_bytes: u64,
}

impl Default for PreviewConfig {
    fn default() -> Self {
        Self {
            renderer_endpoint: None,
            timeout_ms: DEFAULT_PREVIEW_TIMEOUT_MS,
            viewport_width: DEFAULT_PREVIEW_VIEWPORT_WIDTH,
            viewport_height: DEFAULT_PREVIEW_VIEWPORT_HEIGHT,
            cache_entries: DEFAULT_PREVIEW_CACHE_ENTRIES,
            max_png_bytes: DEFAULT_PREVIEW_MAX_PNG_BYTES,
        }
    }
}

impl PreviewConfig {
    /// `true` once a usable renderer endpoint is configured. [lib/preview.js:53]
    #[must_use]
    pub const fn enabled(&self) -> bool {
        self.renderer_endpoint.is_some()
    }

    fn from_source(env: &dyn EnvSource) -> Result<Self, AppError> {
        let renderer_endpoint = match present(env, "PREVIEW_RENDERER_URL") {
            Some(raw) => Some(renderer_endpoint(&raw)?),
            None => None,
        };
        let (viewport_width, viewport_height) = parse_viewport(present(env, "PREVIEW_VIEWPORT"))?;
        Ok(Self {
            renderer_endpoint,
            timeout_ms: positive_integer(
                env,
                "PREVIEW_RENDER_TIMEOUT_MS",
                DEFAULT_PREVIEW_TIMEOUT_MS,
            )?,
            viewport_width,
            viewport_height,
            cache_entries: DEFAULT_PREVIEW_CACHE_ENTRIES,
            max_png_bytes: positive_integer(
                env,
                "PREVIEW_MAX_PNG_BYTES",
                DEFAULT_PREVIEW_MAX_PNG_BYTES,
            )?,
        })
    }
}

/// Port of `rendererEndpoint(value)` — [lib/preview.js:25-36]
fn renderer_endpoint(raw: &str) -> Result<String, AppError> {
    let trimmed = raw.trim();
    let with_slash = if trimmed.ends_with('/') {
        trimmed.to_owned()
    } else {
        format!("{trimmed}/")
    };
    let base = url::Url::parse(&with_slash)
        .map_err(|_| invalid("PREVIEW_RENDERER_URL", trimmed, "an http(s) URL"))?;
    if !matches!(base.scheme(), "http" | "https") {
        return Err(invalid("PREVIEW_RENDERER_URL", trimmed, "an http(s) URL"));
    }
    base.join("render")
        .map(String::from)
        .map_err(|_| invalid("PREVIEW_RENDERER_URL", trimmed, "an http(s) URL"))
}

/// Port of `parseViewport(value)` — [lib/preview.js:16-23]
///
/// Node silently falls back to `1200x630` for a malformed value; a present but malformed
/// value fails closed here instead.
fn parse_viewport(raw: Option<String>) -> Result<(u64, u64), AppError> {
    let Some(raw) = raw else {
        return Ok((
            DEFAULT_PREVIEW_VIEWPORT_WIDTH,
            DEFAULT_PREVIEW_VIEWPORT_HEIGHT,
        ));
    };
    let trimmed = raw.trim();
    let malformed = || {
        invalid(
            "PREVIEW_VIEWPORT",
            trimmed,
            "formatted as \"<width>x<height>\"",
        )
    };
    let lowered = trimmed.to_ascii_lowercase();
    let (width, height) = lowered.split_once('x').ok_or_else(malformed)?;
    if width.is_empty()
        || height.is_empty()
        || !width.bytes().all(|byte| byte.is_ascii_digit())
        || !height.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(malformed());
    }
    let width: u64 = width.parse().map_err(|_| malformed())?;
    let height: u64 = height.parse().map_err(|_| malformed())?;
    // `positiveInteger(match[1], 1200)` restores the default for a zero component.
    Ok((
        if width == 0 {
            DEFAULT_PREVIEW_VIEWPORT_WIDTH
        } else {
            width
        },
        if height == 0 {
            DEFAULT_PREVIEW_VIEWPORT_HEIGHT
        } else {
            height
        },
    ))
}

// ---------------------------------------------------------------------------
// Cloudflare Access identity configuration
// ---------------------------------------------------------------------------

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

    fn from_source(env: &dyn EnvSource) -> Result<Self, AppError> {
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

// ---------------------------------------------------------------------------
// MCP OAuth service-credential configuration
// ---------------------------------------------------------------------------

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
        }
    }
}

impl OAuthConfig {
    /// OAuth is enabled only by a complete issuer/audience/JWKS triple.
    #[must_use]
    pub fn enabled(&self) -> bool {
        !self.issuer.is_empty() && !self.audience.is_empty() && !self.jwks_url.is_empty()
    }

    fn from_source(env: &dyn EnvSource) -> Result<Self, AppError> {
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
        if configured == 3 {
            validate_oauth_url("MCP_OAUTH_ISSUER", &issuer)?;
            validate_oauth_url("MCP_OAUTH_JWKS_URL", &jwks_url)?;
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
        })
    }
}

fn validate_oauth_url(key: &str, value: &str) -> Result<(), AppError> {
    let parsed =
        url::Url::parse(value).map_err(|_| invalid(key, value, "an absolute http(s) URL"))?;
    if !matches!(parsed.scheme(), "http" | "https")
        || parsed.host_str().is_none()
        || parsed.fragment().is_some()
    {
        return Err(invalid(
            key,
            value,
            "an absolute http(s) URL without a fragment",
        ));
    }
    Ok(())
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

// ---------------------------------------------------------------------------
// Seeded publisher keys
// ---------------------------------------------------------------------------

/// One parsed `ARTIFACT_API_KEYS` entry. [lib/db.js:30-58]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SeedKey {
    /// `clientId` component.
    pub client_id: ClientId,
    /// `org` component, defaulting to `default` for two-part entries.
    pub org: OrgId,
    /// The shared secret; hashed by U04/U09 before it reaches the database.
    pub secret: Secret,
}

/// The result of parsing `ARTIFACT_API_KEYS`, including entries Node refuses to seed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SeedKeys {
    /// Entries eligible for bootstrap insertion, in declaration order.
    pub entries: Vec<SeedKey>,
    /// Client ids skipped because they used a documented placeholder secret.
    /// [lib/db.js:38,51-54]
    pub ignored_placeholders: Vec<ClientId>,
}

impl SeedKeys {
    /// Port of the `ARTIFACT_API_KEYS` parsing half of `seedKeysFromEnv`. [lib/db.js:30-58]
    #[must_use]
    pub fn parse(raw: &str) -> Self {
        let mut parsed = Self::default();
        if raw.trim().is_empty() {
            return parsed;
        }
        for entry in raw.split(',') {
            let parts: Vec<&str> = entry.split(':').map(str::trim).collect();
            let (client_id, org, secret) = if parts.len() >= 3 {
                (parts[0], parts[1], parts[2..].join(":"))
            } else if parts.len() == 2 {
                (parts[0], DEFAULT_SEED_KEY_ORG, parts[1].to_owned())
            } else {
                continue;
            };
            if client_id.is_empty() || secret.is_empty() {
                continue;
            }
            if PLACEHOLDER_KEY_SECRETS.contains(&secret.as_str()) {
                parsed.ignored_placeholders.push(ClientId::from(client_id));
                continue;
            }
            let org = if org.is_empty() {
                DEFAULT_SEED_KEY_ORG
            } else {
                org
            };
            parsed.entries.push(SeedKey {
                client_id: ClientId::from(client_id),
                org: OrgId::from(org),
                secret: Secret::new(secret),
            });
        }
        parsed
    }
}

// ---------------------------------------------------------------------------
// AppConfig
// ---------------------------------------------------------------------------

/// Fully validated application configuration.
///
/// `Default` yields exactly the values an empty environment produces, so route tests can
/// build an `AppDeps` without touching the process environment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppConfig {
    /// `PORT` — [server.js:27]
    pub port: u16,
    /// `LISTEN_HOST` — [server.js:190]
    pub listen_host: String,
    /// `PUBLIC_BASE_URL` — [server.js:28]
    pub public_base_url: String,
    /// `DATA_DIR` — [lib/db.js:8]
    pub data_dir: PathBuf,
    /// `APP_NAME` — [lib/config.js:19]
    pub app_name: String,
    /// `APP_BRAND` — [lib/config.js:20]
    pub app_brand: String,
    /// Upload/storage limits.
    pub storage: StorageLimits,
    /// Per-route JSON body limits, in bytes.
    pub body: BodyLimits,
    /// Optional preview renderer configuration.
    pub preview: PreviewConfig,
    /// Cloudflare Access identity configuration.
    pub access: AccessConfig,
    /// Optional OAuth service-credential resource-server configuration.
    pub oauth: OAuthConfig,
    /// `WEBHOOK_ENC_KEY`, validated as canonical 32-byte base64. [lib/crypto.js:9-18]
    pub webhook_enc_key: Option<Secret>,
    /// `ARTIFACT_API_KEYS` bootstrap entries. [lib/db.js:30]
    pub seed_keys: SeedKeys,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self::defaults()
    }
}

impl AppConfig {
    /// The configuration an empty environment produces. Infallible by construction.
    #[must_use]
    pub fn defaults() -> Self {
        Self {
            port: DEFAULT_PORT,
            listen_host: DEFAULT_LISTEN_HOST.to_owned(),
            public_base_url: DEFAULT_PUBLIC_BASE_URL.to_owned(),
            data_dir: PathBuf::from(DEFAULT_DATA_DIR),
            app_name: DEFAULT_APP_NAME.to_owned(),
            app_brand: DEFAULT_APP_BRAND.to_owned(),
            storage: StorageLimits::default(),
            body: BodyLimits::default(),
            preview: PreviewConfig::default(),
            access: AccessConfig {
                clock_tolerance_seconds: DEFAULT_ACCESS_CLOCK_TOLERANCE_SECONDS,
                ..AccessConfig::default()
            },
            oauth: OAuthConfig::default(),
            webhook_enc_key: None,
            seed_keys: SeedKeys::default(),
        }
    }

    /// Parse configuration from the real process environment.
    ///
    /// # Errors
    /// Returns [`AppError::Validation`] when a variable is present but unusable.
    pub fn from_env() -> Result<Self, AppError> {
        Self::from_source(&SystemEnv)
    }

    /// Parse configuration from any [`EnvSource`].
    ///
    /// # Errors
    /// Returns [`AppError::Validation`] when a variable is present but unusable.
    pub fn from_source(env: &dyn EnvSource) -> Result<Self, AppError> {
        let storage = StorageLimits::from_source(env)?;
        let body = BodyLimits::from_source(env, storage.max_bundle_bytes)?;

        let port = match present(env, "PORT") {
            None => DEFAULT_PORT,
            Some(raw) => {
                let trimmed = raw.trim();
                let parsed: u16 = trimmed
                    .parse()
                    .map_err(|_| invalid("PORT", trimmed, "a TCP port between 1 and 65535"))?;
                if parsed == 0 {
                    return Err(invalid("PORT", trimmed, "a TCP port between 1 and 65535"));
                }
                parsed
            }
        };

        let public_base_url = raw_string(env, "PUBLIC_BASE_URL", DEFAULT_PUBLIC_BASE_URL);
        validate_public_base_url(&public_base_url)?;

        Ok(Self {
            port,
            listen_host: non_empty_string(env, "LISTEN_HOST", DEFAULT_LISTEN_HOST),
            public_base_url,
            data_dir: PathBuf::from(raw_string(env, "DATA_DIR", DEFAULT_DATA_DIR)),
            app_name: non_empty_string(env, "APP_NAME", DEFAULT_APP_NAME),
            app_brand: non_empty_string(env, "APP_BRAND", DEFAULT_APP_BRAND),
            storage,
            body,
            preview: PreviewConfig::from_source(env)?,
            access: AccessConfig::from_source(env)?,
            oauth: OAuthConfig::from_source(env)?,
            webhook_enc_key: parse_webhook_enc_key(present(env, "WEBHOOK_ENC_KEY").as_deref())?,
            seed_keys: SeedKeys::parse(&env.get("ARTIFACT_API_KEYS").unwrap_or_default()),
        })
    }

    /// `path.join(dataDir, "artifacts")` — [lib/db.js:9]
    #[must_use]
    pub fn artifact_dir(&self) -> PathBuf {
        self.data_dir.join("artifacts")
    }

    /// `path.join(dataDir, "artifacts.db")` — [lib/db.js:10]
    #[must_use]
    pub fn database_path(&self) -> PathBuf {
        self.data_dir.join("artifacts.db")
    }

    /// `path.join(dataDir, "previews")` — [lib/thumbnails.js:70]
    #[must_use]
    pub fn preview_dir(&self) -> PathBuf {
        self.data_dir.join("previews")
    }

    /// `${PUBLIC_BASE}/${id}` — [server.js:28], [lib/app.js:741]
    #[must_use]
    pub fn artifact_url(&self, id: &ArtifactId) -> String {
        format!("{}/{id}", self.public_base_url.trim_end_matches('/'))
    }

    /// Port of `assertReady()` — [lib/identity.js:56-77].
    ///
    /// # Errors
    /// Returns [`AppError::Validation`] carrying the Node message verbatim when the
    /// configured identity mode is unsafe for the configured bind.
    pub fn validate_startup(&self) -> Result<(), AppError> {
        if self.access.require_jwt && !self.access.jwt_verification_on() {
            return Err(AppError::Validation(
                "REQUIRE_ACCESS_JWT=1 requires both CF_ACCESS_TEAM_DOMAIN and CF_ACCESS_AUD; refusing to start"
                    .to_owned(),
            ));
        }
        if self.access.identity_mode() == AccessIdentityMode::HeaderTrust {
            let host = self.listen_host.trim();
            let loopback = matches!(host, "127.0.0.1" | "::1" | "localhost");
            if !loopback && !self.access.header_trust_allow_insecure {
                return Err(AppError::Validation(format!(
                    "TRUST_ACCESS_HEADERS=1 trusts a spoofable identity header and is unsafe on a non-loopback \
bind ({host}). Set LISTEN_HOST=127.0.0.1 for local dev, configure CF_ACCESS_* for \
production, or set HEADER_TRUST_ALLOW_INSECURE=1 only when a proxy controls host exposure."
                )));
            }
        }
        if !self.oauth.api_keys_enabled && !self.oauth.enabled() {
            return Err(AppError::Validation(
                "MCP_API_KEYS_ENABLED=0 requires a complete MCP OAuth configuration; refusing to start"
                    .to_owned(),
            ));
        }
        Ok(())
    }

    /// Canonical MCP resource identifier used as the OAuth audience.
    #[must_use]
    pub fn mcp_resource_uri(&self) -> String {
        format!("{}/mcp", self.public_base_url.trim_end_matches('/'))
    }

    /// RFC 9728 root discovery document for this deployment.
    #[must_use]
    pub fn oauth_resource_metadata_uri(&self) -> String {
        let mut parsed = url::Url::parse(&self.public_base_url)
            .expect("PUBLIC_BASE_URL is validated during configuration");
        parsed.set_path("/.well-known/oauth-protected-resource");
        parsed.set_query(None);
        parsed.set_fragment(None);
        parsed.into()
    }
}

fn validate_public_base_url(value: &str) -> Result<(), AppError> {
    let parsed = url::Url::parse(value)
        .map_err(|_| invalid("PUBLIC_BASE_URL", value, "an absolute http(s) URL"))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(invalid("PUBLIC_BASE_URL", value, "an absolute http(s) URL"));
    }
    Ok(())
}

/// Port of `parseEncryptionKey(value)` — [lib/crypto.js:9-18].
///
/// The canonical re-encode check rejects non-canonical base64 exactly as Node does.
fn parse_webhook_enc_key(value: Option<&str>) -> Result<Option<Secret>, AppError> {
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

// ---------------------------------------------------------------------------
// Clock
// ---------------------------------------------------------------------------

/// Injected wall clock. Production reads the OS clock; tests pin an instant.
pub trait Clock: Send + Sync + fmt::Debug {
    /// Milliseconds since the Unix epoch.
    fn now_unix_millis(&self) -> i64;

    /// Seconds since the Unix epoch, floored towards negative infinity.
    fn now_unix_seconds(&self) -> i64 {
        self.now_unix_millis().div_euclid(1000)
    }

    /// The SQLite `datetime('now')` rendering (`YYYY-MM-DD HH:MM:SS`, UTC) that every
    /// persisted [`Timestamp`] uses.
    fn now_timestamp(&self) -> Timestamp {
        Timestamp(format_sqlite_datetime(self.now_unix_seconds()))
    }
}

/// Render `seconds` the way SQLite's `datetime('now')` does: UTC, second precision.
#[must_use]
pub fn format_sqlite_datetime(seconds: i64) -> String {
    let moment = OffsetDateTime::from_unix_timestamp(seconds)
        .unwrap_or(OffsetDateTime::UNIX_EPOCH)
        .to_offset(time::UtcOffset::UTC);
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        moment.year(),
        u8::from(moment.month()),
        moment.day(),
        moment.hour(),
        moment.minute(),
        moment.second()
    )
}

/// Production clock reading the host wall clock.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_unix_millis(&self) -> i64 {
        SystemTime::now().duration_since(UNIX_EPOCH).map_or_else(
            |error| {
                i64::try_from(error.duration().as_millis())
                    .map_or(i64::MIN, |millis| millis.saturating_neg())
            },
            |elapsed| i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX),
        )
    }
}

/// Deterministic clock pinned to a fixed instant, advanceable on demand.
#[derive(Debug)]
pub struct FixedClock {
    millis: Mutex<i64>,
}

impl FixedClock {
    /// A clock pinned to `millis` since the Unix epoch.
    #[must_use]
    pub const fn from_millis(millis: i64) -> Self {
        Self {
            millis: Mutex::new(millis),
        }
    }

    /// A clock pinned to `seconds` since the Unix epoch.
    #[must_use]
    pub const fn from_seconds(seconds: i64) -> Self {
        Self::from_millis(seconds * 1000)
    }

    /// Move the clock forward (or backward, with a negative delta).
    pub fn advance_millis(&self, delta: i64) {
        let mut guard = self
            .millis
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        *guard = guard.saturating_add(delta);
    }
}

impl Default for FixedClock {
    fn default() -> Self {
        // 2026-01-01T00:00:00Z — a stable, readable instant for golden output.
        Self::from_seconds(1_767_225_600)
    }
}

impl Clock for FixedClock {
    fn now_unix_millis(&self) -> i64 {
        *self
            .millis
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }
}

// ---------------------------------------------------------------------------
// Randomness
// ---------------------------------------------------------------------------

/// Injected source of random bytes.
///
/// Fallible on purpose: an OS entropy failure must surface as [`AppError::Internal`]
/// rather than a panic, and every caller already returns `Result<_, AppError>`.
pub trait RandomSource: Send + Sync + fmt::Debug {
    /// Fill `dest` with random bytes.
    ///
    /// # Errors
    /// Returns [`AppError::Internal`] when the underlying entropy source fails.
    fn fill_bytes(&self, dest: &mut [u8]) -> Result<(), AppError>;
}

/// Production randomness backed by the operating system CSPRNG.
#[derive(Clone, Copy, Debug, Default)]
pub struct OsRandom;

impl RandomSource for OsRandom {
    fn fill_bytes(&self, dest: &mut [u8]) -> Result<(), AppError> {
        getrandom::fill(dest).map_err(|_| AppError::Internal)
    }
}

/// Deterministic randomness for tests: a seeded xorshift64 stream.
#[derive(Debug)]
pub struct SeededRandom {
    state: Mutex<u64>,
}

impl SeededRandom {
    /// Create a stream from `seed`. A zero seed is remapped because xorshift fixes zero.
    #[must_use]
    pub const fn new(seed: u64) -> Self {
        Self {
            state: Mutex::new(if seed == 0 {
                0x9E37_79B9_7F4A_7C15
            } else {
                seed
            }),
        }
    }
}

impl Default for SeededRandom {
    fn default() -> Self {
        Self::new(0x2026_0720_0000_0001)
    }
}

impl RandomSource for SeededRandom {
    fn fill_bytes(&self, dest: &mut [u8]) -> Result<(), AppError> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        for slot in dest.iter_mut() {
            let mut next = *state;
            next ^= next << 13;
            next ^= next >> 7;
            next ^= next << 17;
            *state = next;
            #[expect(
                clippy::cast_possible_truncation,
                reason = "an eight-bit slice of the state is the intended output"
            )]
            let byte = (next >> 24) as u8;
            *slot = byte;
        }
        Ok(())
    }
}

/// Randomness that replays a fixed byte script, cycling when exhausted.
///
/// Useful for pinning one exact generated identifier in a test.
#[derive(Debug)]
pub struct ScriptedRandom {
    script: Vec<u8>,
    cursor: Mutex<usize>,
}

impl ScriptedRandom {
    /// Create a source replaying `script`. An empty script yields zero bytes.
    #[must_use]
    pub fn new(script: impl Into<Vec<u8>>) -> Self {
        Self {
            script: script.into(),
            cursor: Mutex::new(0),
        }
    }
}

impl RandomSource for ScriptedRandom {
    fn fill_bytes(&self, dest: &mut [u8]) -> Result<(), AppError> {
        if self.script.is_empty() {
            dest.fill(0);
            return Ok(());
        }
        let mut cursor = self
            .cursor
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        for slot in dest.iter_mut() {
            *slot = self.script[*cursor % self.script.len()];
            *cursor = cursor.wrapping_add(1);
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Identifier generation
// ---------------------------------------------------------------------------

/// Injected identifier factory covering every generated id shape in the system.
pub trait IdSource: Send + Sync + fmt::Debug {
    /// A new 12-character artifact id. [lib/store.js:30]
    ///
    /// # Errors
    /// Propagates a [`RandomSource`] failure as [`AppError::Internal`].
    fn artifact_id(&self) -> Result<ArtifactId, AppError>;

    /// A new 24-character public share token. [lib/shares.js:6]
    ///
    /// # Errors
    /// Propagates a [`RandomSource`] failure as [`AppError::Internal`].
    fn share_token(&self) -> Result<ShareToken, AppError>;

    /// A new 16-character feedback id. [lib/feedback.js:10]
    ///
    /// # Errors
    /// Propagates a [`RandomSource`] failure as [`AppError::Internal`].
    fn feedback_id(&self) -> Result<FeedbackId, AppError>;

    /// A new 12-character webhook id. [lib/webhooks.js:11]
    ///
    /// # Errors
    /// Propagates a [`RandomSource`] failure as [`AppError::Internal`].
    fn webhook_id(&self) -> Result<WebhookId, AppError>;
}

/// nanoid's masked rejection sampler, reproduced so alphabets stay uniformly distributed.
///
/// # Errors
/// Propagates a [`RandomSource`] failure, or reports an unusable alphabet/length as
/// [`AppError::Internal`].
pub fn generate_id(
    random: &dyn RandomSource,
    alphabet: &str,
    length: usize,
) -> Result<String, AppError> {
    let symbols = alphabet.as_bytes();
    if symbols.is_empty() || !alphabet.is_ascii() || length == 0 {
        return Err(AppError::Internal);
    }
    // nanoid derives its mask from the next power of two, then rejects out-of-range draws.
    let mask =
        u8::try_from(symbols.len().next_power_of_two() - 1).map_err(|_| AppError::Internal)?;
    let mut out = String::with_capacity(length);
    let mut buffer = [0_u8; 64];
    while out.len() < length {
        random.fill_bytes(&mut buffer)?;
        for byte in buffer {
            if out.len() == length {
                break;
            }
            if let Some(&symbol) = symbols.get((byte & mask) as usize) {
                out.push(char::from(symbol));
            }
        }
    }
    Ok(out)
}

/// Production id factory: nanoid alphabets over an injected [`RandomSource`].
#[derive(Debug)]
pub struct NanoIdSource<R: RandomSource> {
    random: R,
}

impl<R: RandomSource> NanoIdSource<R> {
    /// Wrap `random` as an [`IdSource`].
    #[must_use]
    pub const fn new(random: R) -> Self {
        Self { random }
    }
}

impl Default for NanoIdSource<OsRandom> {
    fn default() -> Self {
        Self::new(OsRandom)
    }
}

impl<R: RandomSource> IdSource for NanoIdSource<R> {
    fn artifact_id(&self) -> Result<ArtifactId, AppError> {
        generate_id(&self.random, ARTIFACT_ID_ALPHABET, ARTIFACT_ID_LENGTH).map(ArtifactId)
    }

    fn share_token(&self) -> Result<ShareToken, AppError> {
        generate_id(&self.random, SHARE_TOKEN_ALPHABET, SHARE_TOKEN_LENGTH).map(ShareToken)
    }

    fn feedback_id(&self) -> Result<FeedbackId, AppError> {
        generate_id(&self.random, FEEDBACK_ID_ALPHABET, FEEDBACK_ID_LENGTH).map(FeedbackId)
    }

    fn webhook_id(&self) -> Result<WebhookId, AppError> {
        generate_id(&self.random, WEBHOOK_ID_ALPHABET, WEBHOOK_ID_LENGTH).map(WebhookId)
    }
}

/// Deterministic id factory: a monotonic counter rendered in each target alphabet.
///
/// Output is unique, ordered, and always satisfies the real validators, so fixtures can
/// pin exact identifiers without pinning a random byte stream.
#[derive(Debug, Default)]
pub struct SequentialIdSource {
    counter: Mutex<u64>,
}

impl SequentialIdSource {
    /// Start the counter at `start` so independent fixtures cannot collide.
    #[must_use]
    pub const fn starting_at(start: u64) -> Self {
        Self {
            counter: Mutex::new(start),
        }
    }

    fn next(&self) -> u64 {
        let mut guard = self
            .counter
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let value = *guard;
        *guard = guard.wrapping_add(1);
        value
    }

    fn render(&self, alphabet: &str, length: usize) -> Result<String, AppError> {
        let symbols = alphabet.as_bytes();
        let first = *symbols.first().ok_or(AppError::Internal)?;
        let radix = symbols.len() as u64;
        let mut value = self.next();
        let mut digits = vec![first; length];
        let mut index = length;
        while index > 0 {
            index -= 1;
            let symbol = *symbols
                .get(usize::try_from(value % radix).map_err(|_| AppError::Internal)?)
                .ok_or(AppError::Internal)?;
            digits[index] = symbol;
            value /= radix;
            if value == 0 {
                break;
            }
        }
        String::from_utf8(digits).map_err(|_| AppError::Internal)
    }
}

impl IdSource for SequentialIdSource {
    fn artifact_id(&self) -> Result<ArtifactId, AppError> {
        self.render(ARTIFACT_ID_ALPHABET, ARTIFACT_ID_LENGTH)
            .map(ArtifactId)
    }

    fn share_token(&self) -> Result<ShareToken, AppError> {
        self.render(SHARE_TOKEN_ALPHABET, SHARE_TOKEN_LENGTH)
            .map(ShareToken)
    }

    fn feedback_id(&self) -> Result<FeedbackId, AppError> {
        self.render(FEEDBACK_ID_ALPHABET, FEEDBACK_ID_LENGTH)
            .map(FeedbackId)
    }

    fn webhook_id(&self) -> Result<WebhookId, AppError> {
        self.render(WEBHOOK_ID_ALPHABET, WEBHOOK_ID_LENGTH)
            .map(WebhookId)
    }
}

// ---------------------------------------------------------------------------
// Testkit
// ---------------------------------------------------------------------------

/// The deterministic adapter bundle later units compose into their fixtures.
///
/// Holding the three sources together keeps every native test reproducible without each
/// unit re-deriving its own fakes.
#[derive(Debug, Default)]
pub struct Testkit {
    /// Default configuration, ready to be pointed at a temporary data directory.
    pub config: AppConfig,
    /// A clock pinned to 2026-01-01T00:00:00Z.
    pub clock: FixedClock,
    /// A counter-backed id factory.
    pub ids: SequentialIdSource,
    /// A seeded, reproducible random byte stream.
    pub random: SeededRandom,
}

impl Testkit {
    /// A testkit whose configuration points at `data_dir`.
    #[must_use]
    pub fn with_data_dir(data_dir: &Path) -> Self {
        let mut kit = Self::default();
        kit.config.data_dir = data_dir.to_path_buf();
        kit
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_environment_matches_declared_defaults() {
        let parsed = AppConfig::from_source(&MapEnv::empty()).expect("defaults parse");
        assert_eq!(parsed, AppConfig::defaults());
        assert_eq!(parsed, AppConfig::default());
    }

    #[test]
    fn mcp_json_limit_follows_bundle_bytes() {
        assert_eq!(mcp_json_limit_for(8 * 1024 * 1024), 50_593_792);
        let parsed = AppConfig::from_source(&MapEnv::empty().with("MAX_BUNDLE_BYTES", "1024"))
            .expect("parse");
        assert_eq!(parsed.body.mcp_json, 1024 * 6 + 256 * 1024);
    }

    #[test]
    fn byte_sizes_match_the_bytes_package() {
        assert_eq!(parse_byte_size("X", "1mb").expect("1mb"), 1_048_576);
        assert_eq!(parse_byte_size("X", "8MB").expect("8MB"), 8_388_608);
        assert_eq!(parse_byte_size("X", "64kb").expect("64kb"), 65_536);
        assert_eq!(parse_byte_size("X", "1.5kb").expect("1.5kb"), 1536);
        assert_eq!(parse_byte_size("X", "1048576").expect("bytes"), 1_048_576);
        assert!(parse_byte_size("X", "banana").is_err());
        assert!(parse_byte_size("X", "-1mb").is_err());
    }

    #[test]
    fn generated_ids_satisfy_the_real_validators() {
        let ids = NanoIdSource::new(SeededRandom::new(7));
        for _ in 0..256 {
            let artifact = ids.artifact_id().expect("artifact id");
            assert!(is_valid_artifact_id(&artifact.0), "{artifact}");
            assert_eq!(artifact.0.len(), ARTIFACT_ID_LENGTH);
            assert!(!RESERVED_ARTIFACT_IDS.contains(&artifact.0.as_str()));
        }
    }

    #[test]
    fn sequential_ids_are_unique_and_in_alphabet() {
        let ids = SequentialIdSource::default();
        let mut seen = BTreeSet::new();
        for _ in 0..1000 {
            let id = ids.artifact_id().expect("artifact id");
            assert!(is_valid_artifact_id(&id.0), "{id}");
            assert!(seen.insert(id.0));
        }
    }

    #[test]
    fn sqlite_datetime_rendering_is_stable() {
        assert_eq!(format_sqlite_datetime(0), "1970-01-01 00:00:00");
        assert_eq!(format_sqlite_datetime(1_767_225_600), "2026-01-01 00:00:00");
        assert_eq!(
            FixedClock::default().now_timestamp(),
            Timestamp("2026-01-01 00:00:00".to_owned())
        );
    }

    #[test]
    fn secrets_never_render_their_value() {
        let secret = Secret::new("super-secret");
        assert_eq!(format!("{secret:?}"), "Secret(<redacted>)");
        assert_eq!(format!("{secret}"), "<redacted>");
        assert_eq!(secret.expose(), "super-secret");
    }
}
