//! Node-derived defaults and limits shared across the configuration sections.

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
/// Viewer-state mutation envelope limit. Values have a separate 256 KiB serialized cap.
pub const DEFAULT_STATE_JSON_LIMIT: u64 = 256 * 1024 + 1024;
pub const STATE_MAX_KEY_BYTES: usize = 64;
pub const STATE_MAX_VALUE_BYTES: usize = 256 * 1024;
pub const STATE_MAX_KEYS: i64 = 64;
/// `express.json({ limit: limits.categoryJson || "8kb" })` — [lib/app.js:634]
pub const DEFAULT_CATEGORY_JSON_LIMIT: u64 = 8 * 1024;
/// The `|| "8mb"` fallback `/mcp` uses when the composition root supplies no limit.
/// `server.js` always supplies `MCP_JSON_LIMIT`, so this is a defence-in-depth floor.
/// [lib/app.js:152]
pub const DEFAULT_MCP_JSON_FALLBACK_LIMIT: u64 = 8 * 1024 * 1024;

/// Origin-side ingress defaults. These are deliberately smaller than the proxy defaults so the
/// service remains safe when it is reached directly during maintenance or a proxy misconfiguration.
pub const DEFAULT_INGRESS_MAX_HEADERS: u64 = 64;
pub const DEFAULT_INGRESS_MAX_HEADER_BYTES: u64 = 32 * 1024;
pub const DEFAULT_INGRESS_MAX_URI_BYTES: u64 = 8 * 1024;
pub const DEFAULT_INGRESS_READ_TIMEOUT_MS: u64 = 15_000;
pub const DEFAULT_INGRESS_READ_HANDLER_TIMEOUT_MS: u64 = 30_000;
pub const DEFAULT_INGRESS_SHUTDOWN_GRACE_MS: u64 = 30_000;
pub const DEFAULT_INGRESS_MAX_CONNECTIONS: u64 = 256;
pub const DEFAULT_INGRESS_MAX_CONCURRENT_REQUESTS: u64 = 128;
pub const DEFAULT_INGRESS_MAX_CONCURRENT_MUTATIONS: u64 = 16;
pub const DEFAULT_INGRESS_RENDER_QUEUE_JOBS: u64 = 64;
pub const DEFAULT_INGRESS_RENDER_QUEUE_BYTES: u64 = 16 * 1024 * 1024;
pub const DEFAULT_INGRESS_JSON_MAX_DEPTH: u64 = 32;
pub const DEFAULT_INGRESS_JSON_MAX_NODES: u64 = 10_000;
pub const DEFAULT_INGRESS_JSON_MAX_MEMBERS: u64 = 1_000;
pub const DEFAULT_INGRESS_JSON_MAX_BATCH: u64 = 100;
pub const DEFAULT_INGRESS_RATE_WINDOW_SECONDS: u64 = 60;
pub const DEFAULT_INGRESS_READS_PER_WINDOW: u64 = 120;
pub const DEFAULT_INGRESS_MUTATIONS_PER_WINDOW: u64 = 30;
pub const DEFAULT_INGRESS_MCP_PER_WINDOW: u64 = 60;
pub const DEFAULT_INGRESS_UPLOADS_PER_WINDOW: u64 = 10;
pub const DEFAULT_INGRESS_FEEDBACK_PER_WINDOW: u64 = 30;
pub const DEFAULT_INGRESS_STATE_PER_WINDOW: u64 = 30;
pub const DEFAULT_INGRESS_ADMIN_PER_WINDOW: u64 = 20;
pub const DEFAULT_INGRESS_VERIFIED_VIEWERS_PER_WINDOW: u64 = 120;
pub const DEFAULT_INGRESS_SHARES_PER_WINDOW: u64 = 60;
pub const DEFAULT_INGRESS_AUTH_FAILURES_PER_WINDOW: u64 = 20;

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
/// Shortest accepted `PREVIEW_RENDERER_TOKEN`, in bytes. The renderer's `RENDER_TOKEN` uses the
/// same rule (`preview-renderer/src/config.rs`).
pub const MIN_PREVIEW_RENDERER_TOKEN_BYTES: usize = 32;
/// Longest accepted `PREVIEW_RENDERER_TOKEN`, in bytes.
pub const MAX_PREVIEW_RENDERER_TOKEN_BYTES: usize = 4096;
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
