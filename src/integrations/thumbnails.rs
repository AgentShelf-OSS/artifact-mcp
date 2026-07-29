//! Owned by U16 (terra) — PNG validation, cache, persistence, and serial priority queue.
//!
//! Digest-addressed thumbnails at `/data/previews/<id>/<body_sha256>.png` (U07's
//! [`crate::artifacts::paths::thumbnail_path`]). **Only validated server-owned artifact ids and
//! SHA-256 digests ever reach a path segment**: every constructor here takes U07's
//! [`SafeArtifactId`]/[`BodyDigest`] newtypes, which are the Rust form of Node's
//! `/^[0-9a-z]{6,24}$/` and `/^[a-f0-9]{64}$/` guards ([lib/thumbnails.js:19-33]).
//!
//! # Optional by design (blueprint risk #11)
//!
//! The renderer sidecar runs under an optional compose profile ([docker-compose.yml:30]) and is
//! usually absent. Nothing in this module returns an error to a caller:
//!
//! * every inherent method returns `Option`/`()` and swallows I/O, transport and validation
//!   failures after logging at `warn`;
//! * the [`PreviewService`] implementation therefore returns `Ok(None)` / `Ok(())` on **every**
//!   path — it has no `Err` construction at all, which is what makes a publish, update or restore
//!   unable to fail because of a preview;
//! * a disabled renderer short-circuits to `None` before any network or filesystem work.
//!
//! This mirrors Node, where `createArtifactPreviewNotifier` delivers the notification with an
//! `undefined`/`null` preview rather than propagating ([lib/preview.js:153-189]) and the startup
//! audit is wrapped in a bare `try/catch` ([server.js:112-122]).
//!
//! # Node oracle, function by function
//!
//! | Rust | Node |
//! |---|---|
//! | [`valid_png`] | `validPng(value, maxBytes)` — [lib/thumbnails.js:35-38] |
//! | [`thumbnail_placeholder`] | `thumbnailPlaceholder(meta, accent)` — [lib/thumbnails.js:52-63] |
//! | [`safe_color`] | `safeColor(value, org)` — [lib/thumbnails.js:40-50] |
//! | [`ThumbnailStore::read_thumbnail`] | `readThumbnail(meta, requestedDigest)` — [lib/thumbnails.js:90-100] |
//! | [`ThumbnailStore::ensure_thumbnail`] | `ensureThumbnail(meta, html)` — [lib/thumbnails.js:142-151] |
//! | `ThumbnailStore::generate` (private) | `generate(meta, html)` — [lib/thumbnails.js:112-140] |
//! | [`ThumbnailStore::cleanup_obsolete`] | `cleanupObsolete(id, keepDigest)` — [lib/thumbnails.js:102-110] |
//! | [`ThumbnailStore::remove_artifact`] | `removeArtifact(id)` — [lib/thumbnails.js:153-156] |
//! | [`ThumbnailStore::audit`] | `audit(artifacts)` — [lib/thumbnails.js:158-193] |
//! | [`ThumbnailQueue`] | `createThumbnailQueue({thumbnails})` — [lib/thumbnails.js:207-236] |
//!
//! # Atomicity
//!
//! A thumbnail is written to `.<digest>.<random>.tmp` inside the artifact's preview directory
//! with `O_EXCL` (Node's `{ flag: "wx" }`, [lib/thumbnails.js:131]) and then `rename`d over the
//! final name. Same-directory rename is atomic on POSIX, so a reader either sees no file or a
//! complete one — never a truncated PNG. A failure at any step removes the temp file and reports
//! `None`; the startup [`ThumbnailStore::audit`] sweeps anything a crash left behind.
//!
//! # Serialisation
//!
//! The sidecar renders one page at a time and answers `503 {"error":"renderer busy"}` to anything
//! concurrent ([preview-renderer/server.js:103-106]). [`ThumbnailQueue`] is the single lane that
//! keeps the client inside that contract, with interactive (`High`) jobs always selected before
//! startup backfill (`Low`) — [lib/thumbnails.js:216-217].
//!
//! # Notes for the units that consume this
//!
//! * **U17** needs only the frozen port: [`PreviewService::read_thumbnail`] for `/thumbnails/:id`
//!   and [`PreviewService::placeholder`] for the SVG fallback ([lib/app.js:176-196]).
//! * **U20** owns startup. Two things are not reachable through the port and need the concrete
//!   types: the audit needs an implementation of [`PreviewArtifactIndex`] over the artifact store
//!   (Node passes `artifactStore`, whose `getArtifactMeta` the audit calls —
//!   [server.js:113]), and the low-priority backfill needs
//!   [`ThumbnailQueue::enqueue`] with [`PreviewHtml::Deferred`], because the port's `html: &str`
//!   cannot express Node's "re-read the body when the job runs, and skip it if the digest moved"
//!   ([server.js:138-143]). Both are ordinary calls on [`PreviewIntegration::store`] /
//!   [`PreviewIntegration::queue`]; no port change is requested.
//! * [`ThumbnailQueue::enqueue`] starts the lane with `tokio::spawn`, so it must be called from
//!   inside a Tokio runtime. Every caller is already an `async` route or startup task.

use std::collections::{HashMap, VecDeque};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use tokio::sync::{OnceCell, oneshot};

use crate::artifacts::paths::{
    BodyDigest, SafeArtifactId, preview_artifact_dir, preview_dir, thumbnail_path,
};
use crate::config::{AppConfig, OsRandom, RandomSource};
use crate::error::AppError;
use crate::model::{ArtifactId, ArtifactMeta};
use crate::ports::BoxFuture;
use crate::ports::integrations::{PreviewPriority, PreviewService};
use crate::security::access::AuthorizedArtifact;

use super::preview::PreviewRenderer;

/// `Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a])` — [lib/thumbnails.js:16]
pub const PNG_SIGNATURE: [u8; 8] = [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a];

/// `export const DEFAULT_MAX_PNG_BYTES = 7_500_000` — [lib/thumbnails.js:17]
pub const DEFAULT_MAX_PNG_BYTES: u64 = 7_500_000;

/// Bytes of randomness in a temporary thumbnail name — `randomBytes(8)`. [lib/thumbnails.js:128]
const TEMP_TOKEN_BYTES: usize = 8;

/// `validPng(value, maxBytes)` — [lib/thumbnails.js:35-38]
///
/// Signature *and* both length bounds, in that order: at least the 8 signature bytes, at most
/// `max_bytes`, and the exact PNG magic. An empty buffer, a JSON error page the sidecar mislabels
/// as `image/png`, and an oversized render are all rejected here.
#[must_use]
pub fn valid_png(value: &[u8], max_bytes: u64) -> bool {
    let length = value.len() as u64;
    length >= PNG_SIGNATURE.len() as u64 && length <= max_bytes && value.starts_with(&PNG_SIGNATURE)
}

/// `safeColor(value, org)` — [lib/thumbnails.js:40-50]
///
/// An explicit `#rgb`/`#rrggbb` accent is honoured (three-digit form expanded); anything else
/// derives a stable hue from the org name with FNV-1a.
///
/// Two JavaScript details are load bearing and are reproduced rather than corrected, because the
/// hue is already being served to browsers today:
///
/// 1. The arithmetic is 32-bit — `hash ^= …` coerces to int32 and `Math.imul` multiplies as
///    int32 — so the port uses `u32` wrapping arithmetic, which is bit-identical.
/// 2. `for (const char of org)` iterates **code points**, but `char.charCodeAt(0)` then reads
///    only the **first UTF-16 code unit** of each one. For an astral character the trailing
///    surrogate is silently dropped: `"🎉"` contributes `0xD83C` alone. Iterating
///    `str::encode_utf16` instead would feed both surrogates in and produce a different hue —
///    verified against the Node oracle in `tests/native/u16_node_parity.rs`.
#[must_use]
pub fn safe_color(accent: Option<&str>, org: &str) -> String {
    let color = accent.unwrap_or_default().trim();
    let hex_digits = color.strip_prefix('#').unwrap_or_default();
    let all_hex = !hex_digits.is_empty() && hex_digits.bytes().all(|byte| byte.is_ascii_hexdigit());
    if all_hex && hex_digits.len() == 6 {
        return color.to_owned();
    }
    if all_hex && hex_digits.len() == 3 {
        let mut expanded = String::with_capacity(7);
        expanded.push('#');
        for character in hex_digits.chars() {
            expanded.push(character);
            expanded.push(character);
        }
        return expanded;
    }

    let mut hash: u32 = 2_166_136_261;
    let mut units = [0_u16; 2];
    for character in org.chars() {
        // `char.charCodeAt(0)`: the first UTF-16 unit of this code point, and only that one.
        let first = character.encode_utf16(&mut units)[0];
        hash ^= u32::from(first);
        hash = hash.wrapping_mul(16_777_619);
    }
    format!("hsl({} 68% 44%)", hash % 360)
}

/// `thumbnailPlaceholder(meta, accent)` — [lib/thumbnails.js:52-63]
///
/// The SVG served whenever no PNG exists: renderer disabled, render still queued, or a bundle
/// (which is never rendered). Byte-identical to Node's template literal, newlines included; the
/// only interpolated values are the two frozen labels and a [`safe_color`] output, so nothing
/// caller-controlled reaches the markup.
#[must_use]
pub fn thumbnail_placeholder(meta: &ArtifactMeta, accent: Option<&str>) -> Vec<u8> {
    let label = if meta.is_bundle { "BUNDLE" } else { "HTML" };
    let detail = if meta.is_bundle {
        "Bundle preview"
    } else {
        "Preview temporarily unavailable"
    };
    let color = safe_color(accent, &meta.org.0);
    format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"1200\" height=\"750\" viewBox=\"0 0 1200 750\" role=\"img\" aria-label=\"{detail}\">\n\
         <rect width=\"1200\" height=\"750\" fill=\"#f4f1ea\"/><rect x=\"56\" y=\"56\" width=\"1088\" height=\"638\" rx=\"18\" fill=\"#fff\" stroke=\"#d8d2c5\" stroke-width=\"3\"/>\n\
         <rect x=\"96\" y=\"96\" width=\"152\" height=\"42\" rx=\"7\" fill=\"{color}\"/><text x=\"172\" y=\"124\" text-anchor=\"middle\" font-family=\"ui-monospace,monospace\" font-size=\"20\" font-weight=\"700\" fill=\"#fff\">{label}</text>\n\
         <path d=\"M96 194h620M96 242h820M96 290h710\" stroke=\"#d8d2c5\" stroke-width=\"22\" stroke-linecap=\"round\"/>\n\
         <circle cx=\"600\" cy=\"474\" r=\"62\" fill=\"{color}\" opacity=\".12\"/><path d=\"M600 442v64M568 474h64\" stroke=\"{color}\" stroke-width=\"10\" stroke-linecap=\"round\"/>\n\
         <text x=\"600\" y=\"586\" text-anchor=\"middle\" font-family=\"system-ui,sans-serif\" font-size=\"27\" fill=\"#596273\">{detail}</text></svg>"
    )
    .into_bytes()
}

// ---------------------------------------------------------------------------
// Audit
// ---------------------------------------------------------------------------

/// The two artifact fields the audit needs to decide whether a preview file is current.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreviewArtifactRef {
    pub is_bundle: bool,
    pub body_sha256: String,
}

/// `artifacts.getArtifactMeta(entry.name)` — [lib/thumbnails.js:165]
///
/// A read-only lookup seam so the audit does not depend on the artifact store. U08/U20 pass the
/// real store at startup; tests pass a map.
pub trait PreviewArtifactIndex: Send + Sync {
    /// Current metadata for `id`, or `None` when no such artifact exists.
    fn artifact(&self, id: &SafeArtifactId) -> Option<PreviewArtifactRef>;
}

impl PreviewArtifactIndex for HashMap<String, PreviewArtifactRef> {
    fn artifact(&self, id: &SafeArtifactId) -> Option<PreviewArtifactRef> {
        self.get(id.as_str()).cloned()
    }
}

/// `{ orphanDirs, partialFiles, invalidFiles }` — [lib/thumbnails.js:159]
///
/// Every listed path has already been removed. Entries are sorted, unlike Node's raw `readdir`
/// order, so the report is reproducible; the removals themselves are identical.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PreviewAuditReport {
    /// Preview directories with no matching artifact, and non-directory entries at that level.
    pub orphan_dirs: Vec<String>,
    /// Files inside a live artifact's directory that are not its current `<digest>.png`,
    /// including interrupted `.tmp` writes.
    pub partial_files: Vec<String>,
    /// Current-digest files that are not valid PNGs.
    pub invalid_files: Vec<String>,
}

impl PreviewAuditReport {
    /// `orphanDirs.length || partialFiles.length || invalidFiles.length` — [server.js:114]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.orphan_dirs.is_empty()
            && self.partial_files.is_empty()
            && self.invalid_files.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Store
// ---------------------------------------------------------------------------

/// One in-flight `ensureThumbnail` shared by every caller of the same `<id>:<digest>` key.
type PendingThumbnail = Arc<OnceCell<Option<Vec<u8>>>>;

/// Digest-addressed thumbnail persistence. `createThumbnailStore({…})` — [lib/thumbnails.js:65-204]
#[derive(Debug)]
pub struct ThumbnailStore {
    data_dir: PathBuf,
    max_png_bytes: u64,
    renderer: Arc<PreviewRenderer>,
    random: Arc<dyn RandomSource>,
    inflight: Mutex<HashMap<String, PendingThumbnail>>,
}

impl ThumbnailStore {
    /// Construct against an explicit data directory.
    #[must_use]
    pub fn new(
        data_dir: impl Into<PathBuf>,
        max_png_bytes: u64,
        renderer: Arc<PreviewRenderer>,
    ) -> Self {
        Self {
            data_dir: data_dir.into(),
            max_png_bytes,
            renderer,
            random: Arc::new(OsRandom),
            inflight: Mutex::new(HashMap::new()),
        }
    }

    /// Construct from validated configuration — `DATA_DIR` and `PREVIEW_MAX_PNG_BYTES`.
    /// [lib/thumbnails.js:66-72]
    #[must_use]
    pub fn from_config(config: &AppConfig, renderer: Arc<PreviewRenderer>) -> Self {
        Self::new(
            config.data_dir.clone(),
            config.preview.max_png_bytes,
            renderer,
        )
    }

    /// Replace the temporary-name entropy source (deterministic in tests).
    #[must_use]
    pub fn with_random(mut self, random: Arc<dyn RandomSource>) -> Self {
        self.random = random;
        self
    }

    /// `enabled: !!renderer?.enabled` — [lib/thumbnails.js:196]
    #[must_use]
    pub fn enabled(&self) -> bool {
        self.renderer.enabled()
    }

    /// `maxPngBytes: maxBytes` — [lib/thumbnails.js:197]
    #[must_use]
    pub const fn max_png_bytes(&self) -> u64 {
        self.max_png_bytes
    }

    /// `path.join(dataDir, "previews")` — [lib/thumbnails.js:71]
    #[must_use]
    pub fn preview_dir(&self) -> PathBuf {
        preview_dir(&self.data_dir)
    }

    /// `readThumbnail(meta, requestedDigest)` — [lib/thumbnails.js:90-100]
    ///
    /// Serves only the *current* revision's digest: a stale `?v=` must fall through to the
    /// placeholder rather than reveal an older render. Bundles never have a thumbnail. A file
    /// that fails validation is deleted and reported missing.
    pub async fn read_thumbnail(
        &self,
        meta: &ArtifactMeta,
        requested_digest: &str,
    ) -> Option<Vec<u8>> {
        if meta.is_bundle || requested_digest != meta.body_sha256 {
            return None;
        }
        let target = self.thumbnail_path(meta)?;
        let max_bytes = self.max_png_bytes;
        blocking(move || {
            let png = std::fs::read(&target).ok()?;
            if valid_png(&png, max_bytes) {
                return Some(png);
            }
            remove_file_quietly(&target);
            None
        })
        .await
        .flatten()
    }

    /// `ensureThumbnail(meta, html)` — [lib/thumbnails.js:142-151]
    ///
    /// Returns the existing thumbnail, renders and persists a new one, or returns `None`. All
    /// concurrent callers for the same `<id>:<digest>` share one render and one write; unlike the
    /// renderer's cache the entry is dropped as soon as it settles (Node's `.finally`), so this
    /// is coalescing only, never a memo of a persisted file.
    ///
    /// `html` is `None` when a deferred backfill job decided the body is stale
    /// ([server.js:138-143]); an existing thumbnail is still returned in that case.
    pub async fn ensure_thumbnail(
        &self,
        meta: &ArtifactMeta,
        html: Option<&str>,
    ) -> Option<Vec<u8>> {
        // `!validArtifactId(meta.id) || !validDigest(meta.body_sha256)` — [lib/thumbnails.js:143]
        if meta.is_bundle {
            return None;
        }
        let id = SafeArtifactId::parse(&meta.id.0)?;
        let digest = BodyDigest::parse(&meta.body_sha256)?;
        let key = format!("{}:{}", id.as_str(), digest.as_str());

        let slot = self.pending_slot(&key);
        let generated = slot
            .get_or_init(|| self.generate(meta, &id, &digest, html))
            .await
            .clone();
        // `.finally(() => inflight.delete(key))` — [lib/thumbnails.js:148]
        self.release_slot(&key, &slot);
        generated
    }

    /// `generate(meta, html)` — [lib/thumbnails.js:112-140]
    async fn generate(
        &self,
        meta: &ArtifactMeta,
        id: &SafeArtifactId,
        digest: &BodyDigest,
        html: Option<&str>,
    ) -> Option<Vec<u8>> {
        if let Some(existing) = self.read_thumbnail(meta, &meta.body_sha256).await {
            return Some(existing);
        }
        // `!renderer?.enabled || typeof html !== "string"` — [lib/thumbnails.js:115]
        if !self.renderer.enabled() {
            return None;
        }
        let html = html?;

        let png = self
            .renderer
            .render_revision_preview(id.as_str(), digest.as_str(), html)
            .await?;
        if !valid_png(&png, self.max_png_bytes) {
            return None;
        }

        if self.persist(id, digest, png.clone()).await {
            Some(png)
        } else {
            None
        }
    }

    /// The temp-write, rename and obsolete sweep of `generate` — [lib/thumbnails.js:125-139]
    ///
    /// `false` means the bytes are not on disk; the caller reports no thumbnail rather than
    /// handing back a PNG a later read would not find.
    async fn persist(&self, id: &SafeArtifactId, digest: &BodyDigest, png: Vec<u8>) -> bool {
        let dir = preview_artifact_dir(&self.data_dir, id);
        let target = thumbnail_path(&self.data_dir, id, digest);
        let Some(token) = self.temp_token() else {
            tracing::warn!(
                artifact = id.as_str(),
                "thumbnail persistence skipped: entropy source unavailable"
            );
            return false;
        };
        // `.${digest}.${randomBytes(8).toString("hex")}.tmp` — [lib/thumbnails.js:128]
        let temporary = dir.join(format!(".{}.{token}.tmp", digest.as_str()));

        let written = blocking(move || {
            let outcome = write_then_rename(&dir, &temporary, &target, &png);
            if let Err(error) = outcome {
                // `await removePath(temporary)` — [lib/thumbnails.js:136]
                remove_file_quietly(&temporary);
                return Err(error.to_string());
            }
            Ok(())
        })
        .await;

        match written {
            Some(Ok(())) => {
                self.cleanup_obsolete(id, digest).await;
                true
            }
            Some(Err(error)) => {
                // `logger.warn?.(…thumbnail persistence failed for ${meta.id}…)`
                // — [lib/thumbnails.js:137]
                tracing::warn!(
                    artifact = id.as_str(),
                    error = %error,
                    "thumbnail persistence failed"
                );
                false
            }
            None => false,
        }
    }

    /// `cleanupObsolete(id, keepDigest)` — [lib/thumbnails.js:102-110]
    ///
    /// Everything in the artifact's preview directory except the current `<digest>.png` — older
    /// revisions and interrupted `.tmp` files alike.
    pub async fn cleanup_obsolete(&self, id: &SafeArtifactId, keep: &BodyDigest) {
        let dir = preview_artifact_dir(&self.data_dir, id);
        let keep_name = format!("{}.png", keep.as_str());
        blocking(move || {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                return;
            };
            for entry in entries.flatten() {
                if entry.file_name() != keep_name.as_str() {
                    remove_any_quietly(&entry.path());
                }
            }
        })
        .await;
    }

    /// `removeArtifact(id)` — [lib/thumbnails.js:153-156]
    ///
    /// Best effort by construction: a delete that cannot clean previews still succeeds.
    pub async fn remove_artifact(&self, id: &ArtifactId) {
        let Some(id) = SafeArtifactId::parse(&id.0) else {
            return;
        };
        let dir = preview_artifact_dir(&self.data_dir, &id);
        blocking(move || remove_any_quietly(&dir)).await;
    }

    /// `audit(artifacts)` — [lib/thumbnails.js:158-193]
    ///
    /// Startup sweep of `/data/previews`: removes directories with no artifact, files that are
    /// not the artifact's current `<digest>.png` (crash-interrupted temporaries included), and
    /// files that are no longer valid PNGs. Never fails — an unwritable previews directory
    /// degrades thumbnails, it does not stop the service ([server.js:111-122]).
    pub async fn audit(&self, index: &dyn PreviewArtifactIndex) -> PreviewAuditReport {
        let root = self.preview_dir();
        let mut report = PreviewAuditReport::default();

        let root_for_create = root.clone();
        blocking(move || {
            let _ = std::fs::create_dir_all(&root_for_create);
        })
        .await;

        for name in read_dir_names(&root).await {
            let target = root.join(&name);
            let id = SafeArtifactId::parse(&name);
            let artifact = id.as_ref().and_then(|id| index.artifact(id));
            let is_dir = is_directory(&target).await;

            let Some(artifact) = artifact.filter(|_| is_dir) else {
                report.orphan_dirs.push(name);
                let orphan = target.clone();
                blocking(move || remove_any_quietly(&orphan)).await;
                continue;
            };

            let expected = (!artifact.is_bundle).then(|| format!("{}.png", artifact.body_sha256));
            for file in read_dir_names(&target).await {
                let full = target.join(&file);
                let label = format!("{name}/{file}");
                let matches = expected.as_deref() == Some(file.as_str()) && is_file(&full).await;
                if !matches {
                    report.partial_files.push(label);
                    let stale = full.clone();
                    blocking(move || remove_any_quietly(&stale)).await;
                    continue;
                }
                let max_bytes = self.max_png_bytes;
                let probe = full.clone();
                let valid = blocking(move || {
                    std::fs::read(&probe).is_ok_and(|png| valid_png(&png, max_bytes))
                })
                .await
                .unwrap_or(false);
                if !valid {
                    report.invalid_files.push(label);
                    blocking(move || remove_file_quietly(&full)).await;
                }
            }
        }

        report.orphan_dirs.sort();
        report.partial_files.sort();
        report.invalid_files.sort();
        report
    }

    /// `thumbnailPath(meta.id, digest)` — [lib/thumbnails.js:80-84], via U07's path builder.
    fn thumbnail_path(&self, meta: &ArtifactMeta) -> Option<PathBuf> {
        let id = SafeArtifactId::parse(&meta.id.0)?;
        let digest = BodyDigest::parse(&meta.body_sha256)?;
        Some(thumbnail_path(&self.data_dir, &id, &digest))
    }

    /// `randomBytes(8).toString("hex")` — [lib/thumbnails.js:128]
    fn temp_token(&self) -> Option<String> {
        let mut bytes = [0_u8; TEMP_TOKEN_BYTES];
        self.random.fill_bytes(&mut bytes).ok()?;
        let mut token = String::with_capacity(TEMP_TOKEN_BYTES * 2);
        for byte in bytes {
            token.push_str(&format!("{byte:02x}"));
        }
        Some(token)
    }

    /// `inflight.has(key) ? inflight.get(key) : …` — [lib/thumbnails.js:147-150]
    fn pending_slot(&self, key: &str) -> PendingThumbnail {
        let mut inflight = self.lock_inflight();
        if let Some(existing) = inflight.get(key) {
            return Arc::clone(existing);
        }
        let slot: PendingThumbnail = Arc::new(OnceCell::new());
        inflight.insert(key.to_owned(), Arc::clone(&slot));
        slot
    }

    /// `.finally(() => inflight.delete(key))` — [lib/thumbnails.js:148]. Pointer-checked so a
    /// later caller's slot survives an earlier caller's cleanup.
    fn release_slot(&self, key: &str, slot: &PendingThumbnail) {
        let mut inflight = self.lock_inflight();
        if inflight
            .get(key)
            .is_some_and(|current| Arc::ptr_eq(current, slot))
        {
            inflight.remove(key);
        }
    }

    fn lock_inflight(&self) -> std::sync::MutexGuard<'_, HashMap<String, PendingThumbnail>> {
        self.inflight
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

// ---------------------------------------------------------------------------
// Blocking filesystem helpers
// ---------------------------------------------------------------------------

/// Run synchronous filesystem work off the async runtime.
///
/// `None` means the blocking pool refused or the closure panicked; callers treat that exactly
/// like an I/O failure, which keeps previews optional even if the runtime is shutting down.
async fn blocking<T, F>(task: F) -> Option<T>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(task).await.ok()
}

/// `mkdir(dir, {recursive}) → writeFile(temp, png, {flag:"wx"}) → rename(temp, target)`
/// — [lib/thumbnails.js:130-132]
///
/// `create_new` is Node's `wx`: it fails rather than overwriting a concurrent writer's temp file.
/// The data is flushed before the rename so the swap publishes complete bytes.
fn write_then_rename(
    dir: &Path,
    temporary: &Path,
    target: &Path,
    png: &[u8],
) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(temporary)?;
    file.write_all(png)?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(temporary, target)
}

/// `rm(target, { force: true })` — [lib/thumbnails.js:86-88]
fn remove_file_quietly(target: &Path) {
    let _ = std::fs::remove_file(target);
}

/// `rm(target, { force: true, recursive: true })` — [lib/thumbnails.js:107], [lib/thumbnails.js:155]
fn remove_any_quietly(target: &Path) {
    if std::fs::remove_file(target).is_ok() {
        return;
    }
    let _ = std::fs::remove_dir_all(target);
}

/// Entry names of `dir`, or an empty list — `try { readdir(dir) } catch { }`.
async fn read_dir_names(dir: &Path) -> Vec<String> {
    let dir = dir.to_path_buf();
    blocking(move || {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return Vec::new();
        };
        let mut names: Vec<String> = entries
            .flatten()
            .filter_map(|entry| entry.file_name().into_string().ok())
            .collect();
        names.sort();
        names
    })
    .await
    .unwrap_or_default()
}

/// `entry.isDirectory()` — [lib/thumbnails.js:167]
async fn is_directory(path: &Path) -> bool {
    let path = path.to_path_buf();
    blocking(move || std::fs::metadata(&path).is_ok_and(|meta| meta.is_dir()))
        .await
        .unwrap_or(false)
}

/// `file.isFile()` — [lib/thumbnails.js:175]
async fn is_file(path: &Path) -> bool {
    let path = path.to_path_buf();
    blocking(move || std::fs::metadata(&path).is_ok_and(|meta| meta.is_file()))
        .await
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Serial priority queue
// ---------------------------------------------------------------------------

/// The HTML a queued job will render.
///
/// `Deferred` is Node's `typeof job.html === "function"` ([lib/thumbnails.js:218]): the startup
/// backfill re-reads the body *at execution time* and returns `None` when a concurrent update has
/// moved the digest on, so a stale body can never overwrite the authoritative thumbnail
/// ([server.js:138-143]).
pub enum PreviewHtml {
    /// Already-loaded body.
    Ready(String),
    /// Body loaded when the job reaches the front of the lane.
    Deferred(Box<dyn FnOnce() -> BoxFuture<'static, Option<String>> + Send>),
}

impl std::fmt::Debug for PreviewHtml {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Ready(html) => formatter
                .debug_struct("Ready")
                .field("bytes", &html.len())
                .finish(),
            Self::Deferred(_) => formatter.write_str("Deferred"),
        }
    }
}

impl From<String> for PreviewHtml {
    fn from(html: String) -> Self {
        Self::Ready(html)
    }
}

struct QueuedJob {
    meta: ArtifactMeta,
    html: PreviewHtml,
    respond: oneshot::Sender<Option<Vec<u8>>>,
}

#[derive(Default)]
struct QueueState {
    high: VecDeque<QueuedJob>,
    low: VecDeque<QueuedJob>,
    running: bool,
}

/// Pending job counts — `pending()` [lib/thumbnails.js:235]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct QueueDepth {
    pub high: usize,
    pub low: usize,
    pub running: bool,
}

/// `createThumbnailQueue({ thumbnails })` — [lib/thumbnails.js:207-236]
///
/// One serial lane: exactly one render is in flight at a time, which is what the sidecar's
/// single-render contract requires ([preview-renderer/server.js:103-106]). Interactive mutation
/// events (`High`) are always selected ahead of startup backfill (`Low`), so publishing an
/// artifact does not queue behind thousands of backfill jobs.
#[derive(Debug)]
pub struct ThumbnailQueue {
    store: Arc<ThumbnailStore>,
    state: Mutex<QueueState>,
}

impl std::fmt::Debug for QueueState {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("QueueState")
            .field("high", &self.high.len())
            .field("low", &self.low.len())
            .field("running", &self.running)
            .finish()
    }
}

impl ThumbnailQueue {
    #[must_use]
    pub fn new(store: Arc<ThumbnailStore>) -> Arc<Self> {
        Arc::new(Self {
            store,
            state: Mutex::new(QueueState::default()),
        })
    }

    /// `pending()` — [lib/thumbnails.js:235]
    #[must_use]
    pub fn depth(&self) -> QueueDepth {
        let state = self.lock_state();
        QueueDepth {
            high: state.high.len(),
            low: state.low.len(),
            running: state.running,
        }
    }

    /// `enqueue(meta, html, { priority })` — [lib/thumbnails.js:228-233]
    ///
    /// The job is registered **before** this returns, so a caller that enqueues `A` then `B`
    /// establishes their relative order regardless of when the returned futures are awaited.
    /// Dropping the returned future does not cancel the job.
    pub fn enqueue(
        self: &Arc<Self>,
        meta: ArtifactMeta,
        html: PreviewHtml,
        priority: PreviewPriority,
    ) -> BoxFuture<'static, Option<Vec<u8>>> {
        let (respond, receive) = oneshot::channel();
        let job = QueuedJob {
            meta,
            html,
            respond,
        };
        let start = {
            let mut state = self.lock_state();
            match priority {
                PreviewPriority::High => state.high.push_back(job),
                PreviewPriority::Low => state.low.push_back(job),
            }
            let idle = !state.running;
            state.running = true;
            idle
        };
        if start {
            // `queueMicrotask(drain)` — [lib/thumbnails.js:231]
            let lane = Arc::clone(self);
            tokio::spawn(lane.drain());
        }
        // A sender dropped without a value (worker panic) resolves to "no thumbnail", never an
        // error: an unfinished preview must not fail the mutation that requested it.
        Box::pin(async move { receive.await.unwrap_or(None) })
    }

    /// `drain()` — [lib/thumbnails.js:212-226]. `high.shift() || low.shift()`.
    async fn drain(self: Arc<Self>) {
        loop {
            let job = {
                let mut state = self.lock_state();
                match state.high.pop_front().or_else(|| state.low.pop_front()) {
                    Some(job) => job,
                    None => {
                        state.running = false;
                        return;
                    }
                }
            };
            let html = match job.html {
                PreviewHtml::Ready(html) => Some(html),
                PreviewHtml::Deferred(load) => load().await,
            };
            let png = self
                .store
                .ensure_thumbnail(&job.meta, html.as_deref())
                .await;
            let _ = job.respond.send(png);
        }
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, QueueState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

// ---------------------------------------------------------------------------
// Port implementation
// ---------------------------------------------------------------------------

/// The frozen [`PreviewService`] adapter: store + serial lane.
///
/// Every method returns `Ok(...)`. There is deliberately no `Err` path — see the module-level
/// "optional by design" note.
#[derive(Debug)]
pub struct PreviewIntegration {
    store: Arc<ThumbnailStore>,
    queue: Arc<ThumbnailQueue>,
}

impl PreviewIntegration {
    #[must_use]
    pub fn new(store: Arc<ThumbnailStore>) -> Self {
        let queue = ThumbnailQueue::new(Arc::clone(&store));
        Self { store, queue }
    }

    /// Build the whole preview stack from configuration. A missing `PREVIEW_RENDERER_URL` yields
    /// a fully inert but perfectly usable service.
    #[must_use]
    pub fn from_config(config: &AppConfig) -> Self {
        let renderer = Arc::new(PreviewRenderer::new(&config.preview));
        Self::new(Arc::new(ThumbnailStore::from_config(config, renderer)))
    }

    #[must_use]
    pub fn store(&self) -> &Arc<ThumbnailStore> {
        &self.store
    }

    #[must_use]
    pub fn queue(&self) -> &Arc<ThumbnailQueue> {
        &self.queue
    }
}

impl PreviewService for PreviewIntegration {
    fn enabled(&self) -> bool {
        self.store.enabled()
    }

    fn read_thumbnail<'a>(
        &'a self,
        artifact: &'a AuthorizedArtifact,
        digest: &'a str,
    ) -> BoxFuture<'a, Result<Option<Vec<u8>>, AppError>> {
        Box::pin(async move { Ok(self.store.read_thumbnail(artifact.meta(), digest).await) })
    }

    fn placeholder(&self, meta: &ArtifactMeta, accent: Option<&str>) -> Vec<u8> {
        thumbnail_placeholder(meta, accent)
    }

    fn ensure_thumbnail<'a>(
        &'a self,
        meta: &'a ArtifactMeta,
        html: &'a str,
        priority: PreviewPriority,
    ) -> BoxFuture<'a, Result<Option<Vec<u8>>, AppError>> {
        Box::pin(async move {
            // The lane exists to serialise *renders*. With no renderer there is nothing to
            // serialise, so the store is consulted directly — which still returns a thumbnail
            // left on disk by an earlier run that had the sidecar enabled, exactly as Node's
            // `generate` does before it checks `renderer?.enabled` ([lib/thumbnails.js:113-115]).
            if !self.store.enabled() {
                return Ok(self.store.ensure_thumbnail(meta, Some(html)).await);
            }
            let job =
                self.queue
                    .enqueue(meta.clone(), PreviewHtml::Ready(html.to_owned()), priority);
            Ok(job.await)
        })
    }

    fn remove_artifact<'a>(&'a self, id: &'a ArtifactId) -> BoxFuture<'a, Result<(), AppError>> {
        Box::pin(async move {
            self.store.remove_artifact(id).await;
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{OrgId, Timestamp};

    fn meta(is_bundle: bool, org: &str) -> ArtifactMeta {
        ArtifactMeta {
            id: ArtifactId("abc123def456".to_owned()),
            client_id: crate::model::ClientId("client".to_owned()),
            org: OrgId(org.to_owned()),
            title: "Title".to_owned(),
            description: String::new(),
            bytes: 0,
            created_at: Timestamp(String::new()),
            updated_at: Timestamp(String::new()),
            uploader_label: String::new(),
            owner_email: None,
            is_bundle,
            entry: String::new(),
            revision: 1,
            category: String::new(),
            hidden: false,
            body_sha256: "a".repeat(64),
        }
    }

    #[test]
    fn png_validation_checks_signature_and_both_bounds() {
        let png = [PNG_SIGNATURE.as_slice(), b"payload"].concat();
        assert!(valid_png(&png, DEFAULT_MAX_PNG_BYTES));
        assert!(valid_png(&PNG_SIGNATURE, DEFAULT_MAX_PNG_BYTES));

        assert!(!valid_png(&[], DEFAULT_MAX_PNG_BYTES), "empty");
        assert!(
            !valid_png(&PNG_SIGNATURE[..7], DEFAULT_MAX_PNG_BYTES),
            "short"
        );
        assert!(
            !valid_png(b"\x89PNG\r\n\x1a\x0b", DEFAULT_MAX_PNG_BYTES),
            "signature"
        );
        assert!(!valid_png(
            b"{\"error\":\"render failed\"}",
            DEFAULT_MAX_PNG_BYTES
        ));
        assert!(!valid_png(&png, png.len() as u64 - 1), "oversized");
        assert!(valid_png(&png, png.len() as u64), "exactly at the cap");
    }

    #[test]
    fn explicit_accents_win_and_everything_else_hashes_the_org() {
        assert_eq!(safe_color(Some("#ABCDEF"), "acme"), "#ABCDEF");
        assert_eq!(safe_color(Some("  #0a0  "), "acme"), "#00aa00");
        assert_eq!(safe_color(Some("#12345"), "acme"), safe_color(None, "acme"));
        assert_eq!(safe_color(Some("red"), "acme"), safe_color(None, "acme"));
        assert!(safe_color(None, "acme").starts_with("hsl("));
        assert_ne!(safe_color(None, "acme"), safe_color(None, "globex"));
    }

    #[test]
    fn the_placeholder_labels_bundles_differently() {
        let single = String::from_utf8(thumbnail_placeholder(&meta(false, "acme"), None))
            .expect("utf-8 svg");
        let bundle =
            String::from_utf8(thumbnail_placeholder(&meta(true, "acme"), None)).expect("utf-8 svg");
        assert!(single.contains(">HTML</text>"));
        assert!(single.contains("Preview temporarily unavailable"));
        assert!(bundle.contains(">BUNDLE</text>"));
        assert!(bundle.contains("Bundle preview"));
        assert!(single.starts_with("<svg xmlns=\"http://www.w3.org/2000/svg\""));
        assert!(single.ends_with("</svg>"));
    }

    #[tokio::test]
    async fn a_disabled_service_never_errors() {
        let renderer = Arc::new(PreviewRenderer::disabled());
        let store = Arc::new(ThumbnailStore::new(
            std::env::temp_dir().join("artifact-mcp-u16-unit-disabled"),
            DEFAULT_MAX_PNG_BYTES,
            renderer,
        ));
        let service = PreviewIntegration::new(store);
        let meta = meta(false, "acme");
        assert!(!service.enabled());
        assert_eq!(
            service
                .ensure_thumbnail(&meta, "<p>hi</p>", PreviewPriority::High)
                .await,
            Ok(None)
        );
        assert_eq!(service.remove_artifact(&meta.id).await, Ok(()));
        assert!(!service.placeholder(&meta, None).is_empty());
    }
}
