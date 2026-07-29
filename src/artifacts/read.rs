//! Owned by U07 (terra) — authorization-gated body reads.
//!
//! Read-only. Staging, swapping, trash, history pruning, and reconciliation are U08's.
//!
//! Node oracle:
//!
//! - `MIME` / `mimeFor` — [lib/store.js:21-27], [lib/store.js:46-48]
//! - `readArtifact` — [lib/store.js:494-501]
//! - `readBundleFile` — [lib/store.js:482-492]
//! - `readHistoryArtifact` — [lib/store.js:583-589]
//! - `readHistoryBundleFile` — [lib/store.js:591-601]
//! - `readTree` — [lib/store.js:527-536]
//!
//! Authorization is *not* performed here. The frozen ports take an `AuthorizedArtifact`
//! (contract §"Frozen domain and access types"), so the caller has already proven access; these
//! functions receive only the validated id and the metadata fields the read depends on.

use std::collections::BTreeMap;
use std::path::Path;

use crate::error::AppError;
use crate::model::ArtifactFile;

use super::paths::{self, SafeArtifactId};
use super::validation::{contained_path, sanitize_relative_path};

/// `MIME` — [lib/store.js:21-27]. Frozen: this exact table is what raw delivery emits.
pub const MIME_TABLE: [(&str, &str); 19] = [
    ("html", "text/html; charset=utf-8"),
    ("htm", "text/html; charset=utf-8"),
    ("css", "text/css; charset=utf-8"),
    ("js", "text/javascript; charset=utf-8"),
    ("mjs", "text/javascript; charset=utf-8"),
    ("json", "application/json"),
    ("svg", "image/svg+xml"),
    ("png", "image/png"),
    ("jpg", "image/jpeg"),
    ("jpeg", "image/jpeg"),
    ("gif", "image/gif"),
    ("webp", "image/webp"),
    ("ico", "image/x-icon"),
    ("woff2", "font/woff2"),
    ("woff", "font/woff"),
    ("ttf", "font/ttf"),
    ("txt", "text/plain; charset=utf-8"),
    ("map", "application/json"),
    ("xml", "application/xml"),
];

/// `|| "application/octet-stream"` — [lib/store.js:47]
pub const DEFAULT_CONTENT_TYPE: &str = "application/octet-stream";

/// The content type every single-file body is served with. A single-file artifact is always
/// HTML by construction (`<id>.html`), and the routes hard-code this string rather than
/// deriving it. [lib/app.js:138], [lib/app.js:447], [lib/app.js:485]
pub const SINGLE_BODY_CONTENT_TYPE: &str = "text/html; charset=utf-8";

/// ``mimeFor(filePath) = MIME[filePath.split(".").pop().toLowerCase()] || "application/octet-stream"``
/// — [lib/store.js:46-48]
///
/// `split(".").pop()` is not "the file extension": a name with no dot yields the whole name,
/// `"a.tar.gz"` yields `gz`, and `"file."` yields the empty string. All three fall through to
/// the default. Reproduced exactly.
///
/// One deliberate, safer divergence: Node indexes a plain object literal, so a bundle file
/// named `x.constructor` or `x.toString` resolves to an inherited `Object.prototype` member
/// instead of `undefined`, and `mimeFor` returns a *function*. Express then throws on the
/// header write. Rust has no prototype chain, so such a name returns
/// [`DEFAULT_CONTENT_TYPE`]. Recorded as a Node bug, not a parity target.
#[must_use]
pub fn mime_for(rel: &str) -> &'static str {
    let extension = rel.rsplit('.').next().unwrap_or("").to_lowercase();
    MIME_TABLE
        .iter()
        .find(|(candidate, _)| *candidate == extension)
        .map_or(DEFAULT_CONTENT_TYPE, |(_, mime)| *mime)
}

// ---------------------------------------------------------------------------
// Single-file bodies
// ---------------------------------------------------------------------------

/// The body half of `readArtifact(id)` — [lib/store.js:494-501]
///
/// Node's `isReserved` gate is already enforced by [`SafeArtifactId::addressable`], and the
/// metadata lookup belongs to the persistence layer, so what remains is: read the file, or
/// report a missing body.
///
/// Node reads with `"utf8"`, which silently substitutes U+FFFD for invalid sequences; this
/// returns the bytes as stored. Publishing only ever writes UTF-8, so the two agree for every
/// body the service itself produced.
#[must_use]
pub fn read_body(artifact_dir: &Path, id: &SafeArtifactId) -> Option<ArtifactFile> {
    read_file_at(
        &paths::single_body_path(artifact_dir, id),
        SINGLE_BODY_CONTENT_TYPE,
    )
}

/// The body half of `readHistoryArtifact(id, revision)` — [lib/store.js:583-589]
///
/// The caller supplies the revision row's `is_bundle`; Node returns `null` for a bundle
/// revision here, which is expressed by the `is_bundle` guard at the call site.
#[must_use]
pub fn read_revision_body(
    artifact_dir: &Path,
    id: &SafeArtifactId,
    revision: u64,
) -> Option<ArtifactFile> {
    read_file_at(
        &paths::history_body_path(artifact_dir, id, revision, false),
        SINGLE_BODY_CONTENT_TYPE,
    )
}

// ---------------------------------------------------------------------------
// Bundle bodies
// ---------------------------------------------------------------------------

/// The body half of `readBundleFile(id, relPath)` — [lib/store.js:482-492]
#[must_use]
pub fn read_bundle_file(
    artifact_dir: &Path,
    id: &SafeArtifactId,
    entry: &str,
    requested: Option<&str>,
) -> Option<ArtifactFile> {
    read_bundle_file_at(&paths::bundle_dir(artifact_dir, id), entry, requested)
}

/// The body half of `readHistoryBundleFile(id, revision, relPath)` — [lib/store.js:591-601]
///
/// `entry` is the *revision row's* entry, not the current metadata's.
#[must_use]
pub fn read_revision_bundle_file(
    artifact_dir: &Path,
    id: &SafeArtifactId,
    revision: u64,
    entry: &str,
    requested: Option<&str>,
) -> Option<ArtifactFile> {
    read_bundle_file_at(
        &paths::history_body_path(artifact_dir, id, revision, true),
        entry,
        requested,
    )
}

/// List every regular file beneath a bundle root in JavaScript's default string-sort order.
/// Sizes are filesystem byte lengths, matching Node's `statSync(full).size`.
pub fn list_bundle_files(root: &Path) -> Result<Option<Vec<(String, u64)>>, AppError> {
    if !root.is_dir() {
        return Ok(None);
    }
    let mut listed = Vec::new();
    collect_bundle_files(root, root, &mut listed).map_err(|_| AppError::Internal)?;
    listed.sort_by(|left, right| left.0.encode_utf16().cmp(right.0.encode_utf16()));
    Ok(Some(listed))
}

fn collect_bundle_files(
    root: &Path,
    dir: &Path,
    listed: &mut Vec<(String, u64)>,
) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let full = entry.path();
        let metadata = std::fs::metadata(&full)?;
        if metadata.is_dir() {
            collect_bundle_files(root, &full, listed)?;
        } else if metadata.is_file() {
            let relative = full
                .strip_prefix(root)
                .unwrap_or(&full)
                .to_string_lossy()
                .replace(std::path::MAIN_SEPARATOR, "/");
            listed.push((relative, metadata.len()));
        }
    }
    Ok(())
}

/// Shared body of both bundle readers; `base` is the bundle directory (live or history).
///
/// ```text
/// const rel = relPath ? sanitizeRel(relPath) : meta.entry;
/// if (!rel) return null;
/// const base = path.resolve(bundleDir(id));
/// const full = path.resolve(path.join(base, rel));
/// if (full !== base && !full.startsWith(base + path.sep)) return null;
/// if (!files.existsSync(full) || !files.statSync(full).isFile()) return null;
/// return { content: files.readFileSync(full), contentType: mimeFor(rel) };
/// ```
/// — [lib/store.js:485-491]
///
/// Note that the stored `entry` is used *unsanitized* when no path is requested: it was
/// sanitized when the bundle was written, and re-sanitizing would not change it.
#[must_use]
pub fn read_bundle_file_at(
    base: &Path,
    entry: &str,
    requested: Option<&str>,
) -> Option<ArtifactFile> {
    let rel = match requested.filter(|value| !value.is_empty()) {
        Some(value) => sanitize_relative_path(value)?,
        None => entry.to_owned(),
    };
    if rel.is_empty() {
        return None;
    }
    let full = contained_path(base, &rel)?;
    read_file_at(&full, mime_for(&rel))
}

/// `readTree(dir)` — [lib/store.js:527-536]
///
/// Reads a bundle snapshot back into `{ 'rel/path': content }`. Node decodes as UTF-8, so
/// invalid sequences become U+FFFD; `from_utf8_lossy` matches that.
///
/// The returned map is ordered lexicographically rather than by directory enumeration. Node's
/// only caller is `restoreById`, which passes an explicit `entry` alongside it
/// ([lib/store.js:617-618]), so entry auto-selection never observes the difference.
///
/// # Errors
/// [`AppError::Internal`] if the snapshot cannot be walked.
pub fn read_tree(root: &Path) -> Result<BTreeMap<String, String>, AppError> {
    let mut out = BTreeMap::new();
    collect_tree(root, root, &mut out).map_err(|_| AppError::Internal)?;
    Ok(out)
}

fn collect_tree(
    root: &Path,
    dir: &Path,
    out: &mut BTreeMap<String, String>,
) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let full = entry.path();
        if std::fs::metadata(&full)?.is_dir() {
            collect_tree(root, &full, out)?;
        } else {
            let rel = full
                .strip_prefix(root)
                .map_err(|_| std::io::Error::other("tree entry escaped its root"))?;
            let key = rel
                .components()
                .map(|component| component.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/");
            let bytes = std::fs::read(&full)?;
            out.insert(key, String::from_utf8_lossy(&bytes).into_owned());
        }
    }
    Ok(())
}

/// `if (!files.existsSync(full) || !files.statSync(full).isFile()) return null;`
/// — [lib/store.js:490]
///
/// A directory, a broken symlink, or an unreadable file all become `None`, exactly as Node's
/// `existsSync`/`isFile` pair does.
fn read_file_at(path: &Path, content_type: &str) -> Option<ArtifactFile> {
    if !std::fs::metadata(path).is_ok_and(|metadata| metadata.is_file()) {
        return None;
    }
    std::fs::read(path).ok().map(|content| ArtifactFile {
        content,
        content_type: content_type.to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_every_frozen_extension() {
        for (extension, mime) in MIME_TABLE {
            assert_eq!(mime_for(&format!("file.{extension}")), mime);
            assert_eq!(
                mime_for(&format!("dir/FILE.{}", extension.to_uppercase())),
                mime
            );
        }
    }

    #[test]
    fn falls_back_for_everything_else() {
        for rel in ["noextension", "archive.tar.gz", "trailing.", "", ".hidden"] {
            assert_eq!(mime_for(rel), DEFAULT_CONTENT_TYPE, "{rel}");
        }
        // No prototype chain in Rust: Node would return `Object.prototype.constructor` here.
        assert_eq!(mime_for("x.constructor"), DEFAULT_CONTENT_TYPE);
        assert_eq!(mime_for("x.toString"), DEFAULT_CONTENT_TYPE);
    }
}
