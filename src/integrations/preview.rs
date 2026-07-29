//! Owned by U16 (terra) — optional preview-renderer HTTP client.
//!
//! This module is a **client of the unchanged Node/Chromium sidecar**
//! (`preview-renderer/server.js`); it does not render anything itself. The sidecar runs under the
//! `preview` compose profile ([docker-compose.yml:30]) and is frequently absent, so every entry
//! point here is total: it returns `None` instead of an error for *any* failure — disabled,
//! unreachable, slow, non-PNG, oversized, or malformed.
//!
//! # Sidecar contract (coded against, verbatim from `preview-renderer/server.js`)
//!
//! | Aspect | Contract | Source |
//! |---|---|---|
//! | Endpoint | `POST <PREVIEW_RENDERER_URL>/render` | [preview-renderer/server.js:99] |
//! | Request | `content-type: application/json`, body `{"html":…,"width":…,"height":…}` | [lib/preview.js:57-63], [preview-renderer/server.js:110-116] |
//! | Health | `GET /health` → `{"status":"ok"}` (not used by the client) | [preview-renderer/server.js:95-98] |
//! | Success | `200` + `content-type: image/png` + `content-length` + the PNG body | [preview-renderer/server.js:27-36], [preview-renderer/server.js:118] |
//! | Busy | `503 {"error":"renderer busy"}` — one render at a time, server side | [preview-renderer/server.js:103-106] |
//! | Bad input | `400 {"error":"html must be a string"}`, `413 {"error":"request too large"}` | [preview-renderer/server.js:111-113], [preview-renderer/server.js:43] |
//! | Failure | `500 {"error":"render failed"}` | [preview-renderer/server.js:120] |
//! | Server timeout | `RENDER_TIMEOUT_MS` (default 7 s), below the client's 8 s | [preview-renderer/server.js:8], [lib/preview.js:6] |
//! | Redirects | never issued; a redirect is treated as a failure | [lib/preview.js:62] |
//!
//! Because the sidecar rejects concurrent renders with `503`, the client must not fan out. The
//! serial lane that guarantees that lives in [`super::thumbnails::ThumbnailQueue`]; this module
//! additionally coalesces *identical* work so a burst of viewers cannot produce a burst of
//! renders.
//!
//! # Node oracle, function by function
//!
//! | Rust | Node |
//! |---|---|
//! | [`PreviewRenderer::new`] | `createPreviewRenderer({…})` — [lib/preview.js:37-49] |
//! | [`PreviewRenderer::enabled`] | `enabled: !!endpoint` — [lib/preview.js:118] |
//! | [`PreviewRenderer::render_preview`] | `renderPreview(html)` — [lib/preview.js:52-94] |
//! | [`PreviewRenderer::render_revision_preview`] | `renderRevisionPreview(id, revision, html)` — [lib/preview.js:103-115] |
//! | cache insert/refresh/evict | `remember(key, value)` — [lib/preview.js:96-101] |
//!
//! Endpoint normalisation (`rendererEndpoint`, [lib/preview.js:25-36]) and the timeout, viewport,
//! cache-size and byte-cap defaults are already parsed by U02 into
//! [`crate::config::PreviewConfig`]; this module consumes them and never reads the environment.
//!
//! # Cache and coalescing semantics
//!
//! Node stores the **promise**, not the resolved buffer ([lib/preview.js:110]), which makes the
//! cache do double duty: concurrent callers with the same key await the same in-flight render, and
//! a settled successful render is reused. A `null` or rejected result is evicted
//! ([lib/preview.js:109-113]) so a transient renderer failure is retried rather than remembered.
//!
//! The Rust port keeps a bounded insertion-ordered list of
//! `Arc<tokio::sync::OnceCell<Option<Vec<u8>>>>` slots. `OnceCell::get_or_init` runs exactly one
//! initialiser for any number of concurrent callers of the same key and hands every other caller
//! the same value, which is precisely the shared-promise behaviour. Failure eviction is explicit
//! and pointer-checked so a slot re-created by a later caller is never removed by an earlier one.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Serialize;
use tokio::sync::OnceCell;

use crate::config::PreviewConfig;

use super::thumbnails::valid_png;

/// The only `content-type` a rendered response may carry. [lib/preview.js:66]
pub const RENDERED_CONTENT_TYPE: &str = "image/png";

/// `JSON.stringify({ html, ...dimensions })` — [lib/preview.js:60]
///
/// Field order matches Node's object literal so a captured request body is byte-comparable.
#[derive(Debug, Serialize)]
struct RenderRequest<'a> {
    html: &'a str,
    width: u64,
    height: u64,
}

/// One cache entry: an in-flight or settled render shared by every caller of the same key.
type RenderSlot = Arc<OnceCell<Option<Vec<u8>>>>;

/// Bounded, insertion-ordered render cache. `Map` iteration order is insertion order in
/// JavaScript, which is what makes Node's delete-then-set an LRU refresh. [lib/preview.js:96-101]
#[derive(Debug, Default)]
struct RenderCache {
    entries: Vec<(String, RenderSlot)>,
}

/// HTTP client for the optional preview sidecar.
///
/// Shared through an [`Arc`]; the cache is shared with it, matching Node's single module-level
/// renderer ([lib/preview.js:124-139]).
#[derive(Debug)]
pub struct PreviewRenderer {
    endpoint: Option<String>,
    client: Option<reqwest::Client>,
    width: u64,
    height: u64,
    timeout: Duration,
    max_png_bytes: u64,
    cache_entries: usize,
    cache: Mutex<RenderCache>,
}

impl PreviewRenderer {
    /// Build a renderer from validated configuration.
    ///
    /// Never fails: a missing `PREVIEW_RENDERER_URL`, or an HTTP client that cannot be
    /// constructed, yields a disabled renderer whose every method is a no-op. That is the
    /// blueprint's optional-by-design requirement — the sidecar is an optional compose profile
    /// and its absence must not be an error anywhere.
    #[must_use]
    pub fn new(config: &PreviewConfig) -> Self {
        let client = reqwest::Client::builder()
            // `redirect: "error"` — [lib/preview.js:62]. `Policy::none` surfaces the 3xx as a
            // non-success status instead of following it, so the outcome is identical.
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_millis(config.timeout_ms))
            .build()
            .map_err(|error| {
                tracing::warn!(
                    error = %error,
                    "preview renderer disabled: HTTP client could not be built"
                );
            })
            .ok();

        let endpoint = match (config.renderer_endpoint.as_ref(), client.as_ref()) {
            (Some(endpoint), Some(_)) => Some(endpoint.clone()),
            _ => None,
        };

        Self {
            endpoint,
            client,
            width: config.viewport_width,
            height: config.viewport_height,
            timeout: Duration::from_millis(config.timeout_ms),
            max_png_bytes: config.max_png_bytes,
            cache_entries: usize::try_from(config.cache_entries)
                .unwrap_or(usize::MAX)
                .max(1),
            cache: Mutex::new(RenderCache::default()),
        }
    }

    /// A renderer with no endpoint: every call is a no-op. Equivalent to `PREVIEW_RENDERER_URL`
    /// being unset, which is the production default.
    #[must_use]
    pub fn disabled() -> Self {
        Self::new(&PreviewConfig::default())
    }

    /// `enabled: !!endpoint` — [lib/preview.js:118]
    #[must_use]
    pub fn enabled(&self) -> bool {
        self.endpoint.is_some()
    }

    /// The byte cap applied to every rendered PNG. [lib/preview.js:49]
    #[must_use]
    pub const fn max_png_bytes(&self) -> u64 {
        self.max_png_bytes
    }

    /// Cache keys, least recently used first. Test observability for the LRU order.
    #[must_use]
    pub fn cache_keys(&self) -> Vec<String> {
        self.lock_cache()
            .entries
            .iter()
            .map(|(key, _)| key.clone())
            .collect()
    }

    /// `renderPreview(html)` — [lib/preview.js:52-94]
    ///
    /// One uncached POST to the sidecar. Returns `None` for a disabled renderer, a transport or
    /// timeout failure, a non-2xx status, a non-`image/png` body, a declared or streamed length
    /// over the cap, or bytes that are not a PNG.
    pub async fn render_preview(&self, html: &str) -> Option<Vec<u8>> {
        let client = self.client.as_ref()?;
        let endpoint = self.endpoint.as_ref()?;

        let mut response = client
            .post(endpoint)
            // `setTimeout(() => controller.abort(…), timeout)` — [lib/preview.js:55]. reqwest's
            // request timeout also spans the body read, matching the abort signal's reach.
            .timeout(self.timeout)
            .json(&RenderRequest {
                html,
                width: self.width,
                height: self.height,
            })
            .send()
            .await
            .ok()?;

        // `if (!response?.ok) return null` — [lib/preview.js:64]
        if !response.status().is_success() {
            return None;
        }

        // `contentType.startsWith("image/png")` — [lib/preview.js:65-66]
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_ascii_lowercase();
        if !content_type.starts_with(RENDERED_CONTENT_TYPE) {
            return None;
        }

        // `declaredLength > maxBytes` — [lib/preview.js:67-68]. A missing or unparsable
        // `content-length` is `NaN` in Node and `None` here: both skip the pre-check and rely on
        // the streaming cap below.
        if response
            .content_length()
            .is_some_and(|declared| declared > self.max_png_bytes)
        {
            return None;
        }

        // `reader.read()` loop with `total > maxBytes` cancellation — [lib/preview.js:70-84].
        // Streaming matters: a hostile or broken renderer must not be able to buffer an unbounded
        // body into this process before the cap is applied.
        let mut png: Vec<u8> = Vec::new();
        let mut total: u64 = 0;
        loop {
            match response.chunk().await {
                Ok(Some(chunk)) => {
                    total = total.saturating_add(chunk.len() as u64);
                    if total > self.max_png_bytes {
                        return None;
                    }
                    png.extend_from_slice(&chunk);
                }
                Ok(None) => break,
                Err(_) => return None,
            }
        }

        // `validPng(png, maxBytes) ? png : null` — [lib/preview.js:88]
        valid_png(&png, self.max_png_bytes).then_some(png)
    }

    /// `renderRevisionPreview(artifactId, revision, html)` — [lib/preview.js:103-115]
    ///
    /// Cached and coalesced by `<artifact-id>:<revision-or-digest>`. Concurrent callers with the
    /// same key share a single render; a `None` result is evicted so the next caller retries.
    pub async fn render_revision_preview(
        &self,
        artifact_id: &str,
        revision: &str,
        html: &str,
    ) -> Option<Vec<u8>> {
        // `if (!endpoint) return Promise.resolve(null)` — [lib/preview.js:104]
        if !self.enabled() {
            return None;
        }
        let key = format!("{artifact_id}:{revision}");
        let slot = self.slot(&key);
        let rendered = slot.get_or_init(|| self.render_preview(html)).await.clone();
        if rendered.is_none() {
            self.forget(&key, &slot);
        }
        rendered
    }

    /// Drop every cached render.
    pub fn clear_cache(&self) {
        self.lock_cache().entries.clear();
    }

    /// `cache.has(key) ? remember(key, cache.get(key)) : remember(key, pending)`
    /// — [lib/preview.js:106-110]
    ///
    /// A hit is moved to the end (LRU refresh); a miss inserts and evicts from the front while
    /// over capacity. The guard is released before the caller awaits, so no lock is ever held
    /// across a render.
    fn slot(&self, key: &str) -> RenderSlot {
        let mut cache = self.lock_cache();
        if let Some(index) = cache
            .entries
            .iter()
            .position(|(existing, _)| existing == key)
        {
            let entry = cache.entries.remove(index);
            let slot = Arc::clone(&entry.1);
            cache.entries.push(entry);
            return slot;
        }
        let slot: RenderSlot = Arc::new(OnceCell::new());
        cache.entries.push((key.to_owned(), Arc::clone(&slot)));
        // `while (cache.size > maxCacheEntries) cache.delete(cache.keys().next().value)`
        // — [lib/preview.js:99]
        while cache.entries.len() > self.cache_entries {
            cache.entries.remove(0);
        }
        slot
    }

    /// `if (cache.get(key) === pending) cache.delete(key)` — [lib/preview.js:110-113]
    ///
    /// The pointer comparison is load bearing: between the failed render and this call another
    /// caller may already have installed a fresh slot for the same key, and evicting *that* would
    /// discard a live in-flight render.
    fn forget(&self, key: &str, slot: &RenderSlot) {
        let mut cache = self.lock_cache();
        if let Some(index) = cache
            .entries
            .iter()
            .position(|(existing, entry)| existing == key && Arc::ptr_eq(entry, slot))
        {
            cache.entries.remove(index);
        }
    }

    /// The cache is a pure memo; a panic while holding it cannot leave a torn invariant, so a
    /// poisoned guard is recovered rather than propagated. Nothing here may panic a caller.
    fn lock_cache(&self) -> std::sync::MutexGuard<'_, RenderCache> {
        self.cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl Default for PreviewRenderer {
    fn default() -> Self {
        Self::disabled()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(endpoint: Option<&str>) -> PreviewConfig {
        PreviewConfig {
            renderer_endpoint: endpoint.map(str::to_owned),
            ..PreviewConfig::default()
        }
    }

    #[test]
    fn a_missing_endpoint_disables_the_client() {
        let renderer = PreviewRenderer::new(&config(None));
        assert!(!renderer.enabled());
        assert!(PreviewRenderer::disabled().cache_keys().is_empty());
    }

    #[test]
    fn a_configured_endpoint_enables_the_client() {
        let renderer = PreviewRenderer::new(&config(Some("http://renderer:3000/render")));
        assert!(renderer.enabled());
        assert_eq!(renderer.max_png_bytes(), 7_500_000);
    }

    #[tokio::test]
    async fn a_disabled_renderer_is_a_no_op() {
        let renderer = PreviewRenderer::new(&config(None));
        assert_eq!(renderer.render_preview("<p>hi</p>").await, None);
        assert_eq!(
            renderer
                .render_revision_preview("abc123def456", "7", "<p>hi</p>")
                .await,
            None
        );
        // A disabled renderer never even allocates a cache slot.
        assert!(renderer.cache_keys().is_empty());
    }

    #[test]
    fn the_cache_refreshes_on_hit_and_evicts_the_oldest_entry() {
        let renderer = PreviewRenderer {
            cache_entries: 2,
            ..PreviewRenderer::new(&config(Some("http://renderer:3000/render")))
        };
        let first = renderer.slot("a:1");
        renderer.slot("b:1");
        assert_eq!(renderer.cache_keys(), vec!["a:1", "b:1"]);

        // A hit moves the key to the most-recent end.
        let again = renderer.slot("a:1");
        assert!(Arc::ptr_eq(&first, &again));
        assert_eq!(renderer.cache_keys(), vec!["b:1", "a:1"]);

        // Inserting past the bound evicts the least recently used key.
        renderer.slot("c:1");
        assert_eq!(renderer.cache_keys(), vec!["a:1", "c:1"]);
    }

    #[test]
    fn eviction_is_pointer_checked() {
        let renderer = PreviewRenderer::new(&config(Some("http://renderer:3000/render")));
        let stale = renderer.slot("a:1");
        renderer.forget("a:1", &stale);
        assert!(renderer.cache_keys().is_empty());

        // A slot re-created after the failure must survive the loser's eviction.
        let fresh = renderer.slot("a:1");
        renderer.forget("a:1", &stale);
        assert_eq!(renderer.cache_keys(), vec!["a:1"]);
        assert!(!Arc::ptr_eq(&stale, &fresh));
        renderer.clear_cache();
        assert!(renderer.cache_keys().is_empty());
    }
}
