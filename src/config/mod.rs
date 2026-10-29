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

mod access;
mod cast;
mod clock;
mod credentials;
mod defaults;
mod env;
mod id_source;
mod ids;
mod ingress;
mod limits;
mod oauth;
mod parse;
mod preview;
mod random;
mod seed_keys;
mod stale_guard;
#[cfg(any(test, feature = "test-util"))]
mod testkit;
mod web_push;

#[cfg(test)]
mod tests;

pub use access::{AccessConfig, AccessIdentityMode};
pub use clock::{Clock, SystemClock, format_sqlite_datetime};
pub use defaults::*;
pub use env::{EnvSource, MapEnv, Secret, SystemEnv};
pub use id_source::{IdSource, NanoIdSource, generate_id};
pub use ids::{
    ARTIFACT_ID_ALPHABET, ARTIFACT_ID_LENGTH, FEEDBACK_ID_ALPHABET, FEEDBACK_ID_LENGTH,
    RESERVED_ARTIFACT_IDS, SHARE_TOKEN_ALPHABET, SHARE_TOKEN_LENGTH, WEBHOOK_ID_ALPHABET,
    WEBHOOK_ID_LENGTH, is_valid_artifact_id, is_valid_webhook_id,
};
pub use ingress::IngressConfig;
pub use limits::{BodyLimits, StorageLimits};
pub use oauth::OAuthConfig;
pub use preview::PreviewConfig;
pub use random::{OsRandom, RandomSource};
pub use seed_keys::{SeedKey, SeedKeys};
pub use stale_guard::{
    DEFAULT_STALE_GUARD_MIN_SEGMENTS, DEFAULT_STALE_GUARD_MIN_SHARE, StaleGuardConfig,
    StaleGuardMode,
};
// Deterministic fakes for tests. The `test-util` feature exposes them to the integration tests in
// `tests/`; release builds do not compile them.
#[cfg(any(test, feature = "test-util"))]
pub use clock::FixedClock;
#[cfg(any(test, feature = "test-util"))]
pub use id_source::SequentialIdSource;
#[cfg(any(test, feature = "test-util"))]
pub use random::{ScriptedRandom, SeededRandom};
#[cfg(any(test, feature = "test-util"))]
pub use testkit::Testkit;
pub use web_push::{
    DEFAULT_WEB_PUSH_ENDPOINT_HOSTS, VapidIdentity, WebPushConfig, valid_web_push_subject,
    vapid_public_key,
};

use std::{collections::BTreeSet, path::PathBuf};

use crate::{error::AppError, model::ArtifactId};

use access::parse_access_sync;
use cast::parse_cast_ids;
use credentials::{parse_audit_ledger_hmac_key, parse_discord_bot_token, parse_webhook_enc_key};
use parse::{disabled_unless_one, invalid, non_empty_string, present, raw_string};

/// Fully validated application configuration.
///
/// `Default` yields exactly the values an empty environment produces, so route tests can
/// build an `AppDeps` without touching the process environment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppConfig {
    pub action_grants: Vec<crate::actions::ActionGrant>,
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
    /// Request admission, complexity, and trusted-proxy controls.
    pub ingress: IngressConfig,
    /// Optional preview renderer configuration.
    pub preview: PreviewConfig,
    /// Cloudflare Access identity configuration.
    pub access: AccessConfig,
    /// Optional reconciliation of explicit organization emails to one dedicated Access policy.
    pub access_sync: Option<crate::integrations::access_sync::AccessSyncConfig>,
    /// Optional OAuth service-credential resource-server configuration.
    pub oauth: OAuthConfig,
    /// `WEBHOOK_ENC_KEY`, validated as canonical 32-byte base64. [lib/crypto.js:9-18]
    pub webhook_enc_key: Option<Secret>,
    /// `DISCORD_BOT_TOKEN`, used only for Discord's authenticated thread-management REST calls.
    /// The token is never rendered, logged, or persisted.
    pub discord_bot_token: Option<Secret>,
    /// `DISCORD_INBOUND_ENABLED` — incident kill switch for the optional Gateway integration.
    /// It defaults off; local feedback and outbound durable delivery do not depend on it.
    pub discord_inbound_enabled: bool,
    /// `AUDIT_LEDGER_HMAC_KEY`, a canonical base64 HMAC key. The executable refuses to serve
    /// without it; keeping this optional here preserves deterministic route/unit fixtures.
    pub audit_ledger_hmac_key: Option<Secret>,
    /// `ARTIFACT_API_KEYS` bootstrap entries. [lib/db.js:30]
    pub seed_keys: SeedKeys,
    /// Exact artifact ID and current revision grants for the trusted casting integration.
    pub cast_ids: BTreeSet<(String, u64)>,
    /// ADR-0012 Web Push reminders. See [`AppConfig::web_push_enabled`].
    pub web_push: WebPushConfig,
    /// ADR-0023 stale-base guard for full-content updates.
    pub stale_guard: StaleGuardConfig,
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
            action_grants: vec![],
            port: DEFAULT_PORT,
            listen_host: DEFAULT_LISTEN_HOST.to_owned(),
            public_base_url: DEFAULT_PUBLIC_BASE_URL.to_owned(),
            data_dir: PathBuf::from(DEFAULT_DATA_DIR),
            app_name: DEFAULT_APP_NAME.to_owned(),
            app_brand: DEFAULT_APP_BRAND.to_owned(),
            storage: StorageLimits::default(),
            body: BodyLimits::default(),
            ingress: IngressConfig::default(),
            preview: PreviewConfig::default(),
            access: AccessConfig {
                clock_tolerance_seconds: DEFAULT_ACCESS_CLOCK_TOLERANCE_SECONDS,
                ..AccessConfig::default()
            },
            oauth: OAuthConfig::default(),
            access_sync: None,
            webhook_enc_key: None,
            discord_bot_token: None,
            discord_inbound_enabled: false,
            audit_ledger_hmac_key: None,
            seed_keys: SeedKeys::default(),
            cast_ids: BTreeSet::new(),
            web_push: WebPushConfig::default(),
            stale_guard: StaleGuardConfig::default(),
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
        // The speech module reads this lazily; reject invalid values at startup instead.
        if let Some(raw) = present(env, "TTS_WORKER_CONCURRENCY") {
            crate::http::routes::speech::parse_worker_concurrency(Some(&raw)).map_err(|_| {
                invalid(
                    "TTS_WORKER_CONCURRENCY",
                    raw.trim(),
                    "an integer from 1 to 8",
                )
            })?;
        }
        if let Some(raw) = present(env, "POCKET_TTS_EXTRA_VOICES") {
            crate::http::routes::speech::parse_extra_pocket_voices(Some(&raw)).map_err(|_| {
                invalid(
                    "POCKET_TTS_EXTRA_VOICES",
                    "<invalid>",
                    "a comma-separated list of up to 32 valid id:Label[:gainDb] entries",
                )
            })?;
        }

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
            action_grants: crate::actions::load_grants(
                present(env, "ARTIFACT_ACTIONS_FILE").as_deref(),
            )?,
            port,
            listen_host: non_empty_string(env, "LISTEN_HOST", DEFAULT_LISTEN_HOST),
            public_base_url,
            data_dir: PathBuf::from(raw_string(env, "DATA_DIR", DEFAULT_DATA_DIR)),
            app_name: non_empty_string(env, "APP_NAME", DEFAULT_APP_NAME),
            app_brand: non_empty_string(env, "APP_BRAND", DEFAULT_APP_BRAND),
            storage,
            body,
            ingress: IngressConfig::from_source(env)?,
            preview: PreviewConfig::from_source(env)?,
            access: AccessConfig::from_source(env)?,
            access_sync: parse_access_sync(env)?,
            oauth: OAuthConfig::from_source(env)?,
            webhook_enc_key: parse_webhook_enc_key(present(env, "WEBHOOK_ENC_KEY").as_deref())?,
            discord_bot_token: parse_discord_bot_token(
                present(env, "DISCORD_BOT_TOKEN").as_deref(),
            )?,
            discord_inbound_enabled: disabled_unless_one(env, "DISCORD_INBOUND_ENABLED")?,
            audit_ledger_hmac_key: parse_audit_ledger_hmac_key(
                present(env, "AUDIT_LEDGER_HMAC_KEY").as_deref(),
            )?,
            seed_keys: SeedKeys::parse(&env.get("ARTIFACT_API_KEYS").unwrap_or_default()),
            cast_ids: parse_cast_ids(present(env, "ARTIFACT_CAST_IDS").as_deref())?,
            web_push: WebPushConfig::from_source(env)?,
            stale_guard: StaleGuardConfig::from_source(env),
        })
    }

    /// Web Push reminders are enabled only with a valid VAPID identity **and** `WEBHOOK_ENC_KEY`,
    /// which encrypts subscription endpoints at rest. [lib/config.js] `parseWebPushConfig`.
    #[must_use]
    pub fn web_push_enabled(&self) -> bool {
        self.web_push.vapid.is_some() && self.webhook_enc_key.is_some()
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
