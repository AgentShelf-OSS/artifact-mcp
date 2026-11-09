//! Artifact input, bundle path, MIME, and limit validation.
//!
//! These rules preserve the frozen input contract, originally defined by the retired Node twin:
//!
//! - relative path sanitization
//! - reserved artifact ids
//! - artifact id and SHA-256 shape checks
//! - single-file body validation
//! - bundle validation
//!
//! Surprising historical behavior is preserved and called out in a comment.
//! Conformance checks the frozen outputs, so changing these rules requires a contract change.
//! Compatibility constraints are listed below.
//!
//! # Compatibility constraints
//!
//! 1. `RESERVED` is a hand-maintained list that has drifted from the real route table. The router registers top-level `/thumbnails/:id` and `/notifications/seen` before the `/:id` artifact routes, but neither `thumbnails` nor `notifications` is in `RESERVED`. Both are valid artifact-id shapes under `/^[0-9a-z]{6,24}$/`. Ported faithfully; see [`is_reserved_artifact_id`].
//! 2. Drive-qualified and UNC-looking paths are accepted as *literal relative names*. `C:\x` normalises to `C:/x` and `\\srv\share\x` to `srv/share/x`. Neither escapes the bundle root on a POSIX host, so this is a naming oddity rather than a traversal hole, but it is not what a reader of the sanitizer expects. Preserved for compatibility; see [`sanitize_relative_path`].
//! 3. Entry auto-selection picks the first `.html` file in publisher order. A bundle with `{"z.html": …, "a.html": …}` and no `index.html` or explicit entry must resolve to `z.html`. Both `ArtifactContent::Bundle::files` and [`validate_bundle`] preserve that order; a sorted container would select `a.html` and break compatibility.

use std::path::{Path, PathBuf};

use crate::config::{RESERVED_ARTIFACT_IDS, StorageLimits, is_valid_artifact_id};
use crate::error::AppError;
use crate::model::ArtifactId;

/// The default bundle entry, checked before scanning for any `.html` file.
pub const DEFAULT_BUNDLE_ENTRY: &str = "index.html";

/// Length of a lowercase hex SHA-256 digest — `const SHA256 = /^[a-f0-9]{64}$/`.
pub const DIGEST_HEX_LENGTH: usize = 64;

// ---------------------------------------------------------------------------
// Identifiers
// ---------------------------------------------------------------------------

/// `RESERVED.has(id) || !/^[0-9a-z]{6,24}$/.test(id)`
///
/// The retired Node twin's `RESERVED` set contained the empty string; an empty id fails the shape
/// test, so `RESERVED_ARTIFACT_IDS` omits it without changing the answer.
///
/// **Known divergence from the route table (contract-delta request 1):** `thumbnails` and
/// `notifications` are real top-level routes and are *not* reserved. Reproduced as-is.
#[must_use]
pub fn is_reserved_artifact_id(id: &str) -> bool {
    RESERVED_ARTIFACT_IDS.contains(&id) || !is_valid_artifact_id(id)
}

/// An artifact id that has passed `/^[0-9a-z]{6,24}$/` — the only value a filesystem path may
/// be built from, including paths in the preview store.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SafeArtifactId(String);

impl SafeArtifactId {
    /// Accepts any id with a valid *shape*, including the reserved names.
    ///
    /// Path construction checks the id's shape without checking reserved names.
    /// Use [`Self::addressable`] when resolving an id supplied by a caller
    /// to read an artifact or its metadata.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        is_valid_artifact_id(value).then(|| Self(value.to_owned()))
    }

    /// Accepts only ids that can actually address an artifact — shape *and* not reserved.
    #[must_use]
    pub fn addressable(value: &str) -> Option<Self> {
        (!is_reserved_artifact_id(value)).then(|| Self(value.to_owned()))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<&ArtifactId> for SafeArtifactId {
    type Error = AppError;

    fn try_from(value: &ArtifactId) -> Result<Self, Self::Error> {
        Self::parse(&value.0).ok_or(AppError::ConcealedNotFound)
    }
}

/// A validated lowercase hex SHA-256 digest — `const SHA256 = /^[a-f0-9]{64}$/`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BodyDigest(String);

impl BodyDigest {
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        is_valid_digest(value).then(|| Self(value.to_owned()))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// `SHA256.test(String(value || ""))`
#[must_use]
pub fn is_valid_digest(value: &str) -> bool {
    value.len() == DIGEST_HEX_LENGTH
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

// ---------------------------------------------------------------------------
// Path sanitizing
// ---------------------------------------------------------------------------

/// Port of `path.posix.normalize`, restricted to the relative inputs `sanitizeRel` produces.
///
/// Keep leading `..` segments for relative paths, collapse `.` and repeated separators,
/// and re-append a trailing separator when the input had one.
/// An input that normalises away entirely becomes `"."`, or `"./"` when the
/// input ended in a separator.
#[must_use]
pub fn posix_normalize_relative(value: &str) -> String {
    let trailing_separator = value.ends_with('/');
    let mut stack: Vec<&str> = Vec::new();
    for segment in value.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                if matches!(stack.last(), Some(&last) if last != "..") {
                    stack.pop();
                } else {
                    stack.push("..");
                }
            }
            other => stack.push(other),
        }
    }
    if stack.is_empty() {
        // No segments remain: return "./" or "." for the relative path.
        return if trailing_separator { "./" } else { "." }.to_owned();
    }
    let mut normalized = stack.join("/");
    if trailing_separator {
        normalized.push('/');
    }
    normalized
}

/// `sanitizeRel(value)`
///
/// ```text
/// const normalized = path.posix.normalize(
///   String(value || "").replace(/\\/g, "/").replace(/^\/+/, ""));
/// if (!normalized || normalized === "." || normalized === ".."
///     || normalized.startsWith("../") || path.posix.isAbsolute(normalized)) return null;
/// if (normalized.split("/").some((segment) => segment === "..")) return null;
/// return normalized;
/// ```
///
/// Consequences of the frozen path rules:
///
/// - A leading `/` is *stripped*, not rejected: `/etc/passwd` becomes the literal relative
///   name `etc/passwd`. It cannot escape the bundle root.
/// - Backslashes become separators first, so `..\..\x` is rejected as traversal, while
///   `C:\x` survives as the literal relative name `C:/x` (contract-delta request 2).
/// - Percent-encoded traversal (`..%2f..`) is *not* decoded, so it stays a single literal
///   filename. Safe, and deliberately preserved.
/// - A trailing separator survives (`a/b/`), and a bare `./` is accepted. The digest and
///   the on-disk manifest already encode these historical quirks.
#[must_use]
pub fn sanitize_relative_path(value: &str) -> Option<String> {
    let replaced = value.replace('\\', "/");
    let stripped = replaced.trim_start_matches('/');
    let normalized = posix_normalize_relative(stripped);
    if normalized.is_empty()
        || normalized == "."
        || normalized == ".."
        || normalized.starts_with("../")
        || normalized.starts_with('/')
    {
        return None;
    }
    if normalized.split('/').any(|segment| segment == "..") {
        return None;
    }
    Some(normalized)
}

/// Resolve a path purely lexically, without touching the filesystem or
/// following symlinks. Used for the bundle-file containment check.
#[must_use]
pub fn lexically_resolve(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(path)
    };
    let mut resolved = PathBuf::new();
    for component in absolute.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                resolved.pop();
            }
            other => resolved.push(other),
        }
    }
    resolved
}

/// `const base = path.resolve(dir); const full = path.resolve(path.join(base, rel));`
/// `if (full !== base && !full.startsWith(base + path.sep)) return null;`
///
/// `rel` must already have passed [`sanitize_relative_path`]; this is the second, independent
/// gate that keeps a sanitizer regression from becoming exploitable.
#[must_use]
pub fn contained_path(base: &Path, rel: &str) -> Option<PathBuf> {
    let base = lexically_resolve(base);
    let full = lexically_resolve(&base.join(rel));
    (full == base || full.starts_with(&base)).then_some(full)
}

// ---------------------------------------------------------------------------
// Body validation
// ---------------------------------------------------------------------------

/// ECMAScript `String.prototype.trim` whitespace: `WhiteSpace` + `LineTerminator`.
///
/// Deliberately *not* `char::is_whitespace`: Rust follows the Unicode `White_Space` property,
/// which includes U+0085 (which JS excludes) and excludes U+FEFF (which JS includes). The
/// difference decides whether a body counts as blank and is therefore rejected.
const fn is_js_whitespace(value: char) -> bool {
    matches!(
        value,
        '\u{9}'..='\u{d}'
            | '\u{20}'
            | '\u{a0}'
            | '\u{1680}'
            | '\u{2000}'..='\u{200a}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202f}'
            | '\u{205f}'
            | '\u{3000}'
            | '\u{feff}'
    )
}

/// `String.prototype.trim` — see `is_js_whitespace`.
#[must_use]
pub fn js_trim(value: &str) -> &str {
    value.trim_matches(is_js_whitespace)
}

/// A single-file body that passed `publish`'s guards.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValidatedSingleBody {
    /// `Buffer.byteLength(html, "utf8")`
    pub bytes: u64,
    /// `sha256(html)`
    pub body_sha256: String,
}

/// ```text
/// if (typeof html !== "string" || !html.trim()) throw new Error("html is required");
/// const bytes = Buffer.byteLength(html, "utf8");
/// const bodySha256 = sha256(html);
/// if (bytes > maxBytes) throw new Error(`html exceeds ${maxBytes} bytes (got ${bytes})`);
/// ```
///
/// The order matters: a blank oversized body reports `html is required`, not the size error.
/// The limit is exclusive — exactly `maxBytes` is accepted.
///
/// # Errors
/// [`AppError::Validation`] carrying the frozen validation message.
pub fn validate_single_body(
    html: &str,
    limits: &StorageLimits,
) -> Result<ValidatedSingleBody, AppError> {
    if js_trim(html).is_empty() {
        return Err(AppError::Validation("html is required".to_owned()));
    }
    let bytes = html.len() as u64;
    let max = limits.max_artifact_bytes;
    if bytes > max {
        return Err(AppError::Validation(format!(
            "html exceeds {max} bytes (got {bytes})"
        )));
    }
    Ok(ValidatedSingleBody {
        bytes,
        body_sha256: super::digest::sha256_hex(html.as_bytes()),
    })
}

/// A bundle snapshot that passed `validateBundle`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValidatedBundle {
    /// `clean` — sanitized relative path and content, in the caller's original order.
    /// Duplicates are preserved: two raw names can sanitize to the same relative path, and
    /// both are kept, counting both byte lengths and writing the later one last.
    pub files: Vec<(String, String)>,
    /// `total` — sum of `Buffer.byteLength(content, "utf8")`.
    pub total_bytes: u64,
    /// `selectedEntry`
    pub entry: String,
}

/// `validateBundle(bundleFiles, entry, preferEntry)`
///
/// `files` preserves publisher order, as required by the frozen input contract, and
/// entry auto-selection depends on that order. See contract-delta request 3.
///
/// `prefer_entry` is the existing `meta.entry` on an update; pass `None`
/// for a fresh publish, which is exactly what `publishBundle` does.
///
/// # Errors
/// [`AppError::Validation`] carrying the frozen message, in this evaluation order:
/// empty → too many files → unsafe path → total bytes → entry resolution.
pub fn validate_bundle(
    files: &[(String, String)],
    entry: Option<&str>,
    prefer_entry: Option<&str>,
    limits: &StorageLimits,
) -> Result<ValidatedBundle, AppError> {
    if files.is_empty() {
        return Err(AppError::Validation("files is empty".to_owned()));
    }
    let max_files = limits.max_bundle_files;
    if files.len() as u64 > max_files {
        return Err(AppError::Validation(format!(
            "too many files (max {max_files})"
        )));
    }

    let mut total: u64 = 0;
    let mut clean: Vec<(String, String)> = Vec::with_capacity(files.len());
    for (raw, content) in files {
        let Some(rel) = sanitize_relative_path(raw) else {
            return Err(AppError::Validation(format!("unsafe file path: {raw}")));
        };
        total = total.saturating_add(content.len() as u64);
        clean.push((rel, content.clone()));
    }

    let max_bytes = limits.max_bundle_bytes;
    if total > max_bytes {
        return Err(AppError::Validation(format!(
            "bundle exceeds {max_bytes} bytes (got {total})"
        )));
    }

    let entry = select_entry(&clean, entry, prefer_entry)?;
    Ok(ValidatedBundle {
        files: clean,
        total_bytes: total,
        entry,
    })
}

/// ```text
/// let selectedEntry = entry ? sanitizeRel(entry) : "";
/// if (selectedEntry && !relativePaths.has(selectedEntry)) throw …;
/// if (!selectedEntry && preferEntry && relativePaths.has(preferEntry)) selectedEntry = preferEntry;
/// if (!selectedEntry) selectedEntry = relativePaths.has("index.html")
///   ? "index.html"
///   : clean.map(([rel]) => rel).find((rel) => rel.endsWith(".html"));
/// if (!selectedEntry) throw new Error("no HTML entry found — include index.html or pass an 'entry'");
/// ```
///
/// Note the fall-through: a *requested* entry that fails `sanitizeRel` yields `null`, which is
/// falsy in the historical contract, so auto-select instead of reporting the bad entry.
///
/// `.endsWith(".html")` is literal — a `.htm` file never auto-selects.
///
/// # Errors
/// [`AppError::Validation`] with `entry "…" is not one of the files` or the no-entry message.
pub fn select_entry(
    clean: &[(String, String)],
    entry: Option<&str>,
    prefer_entry: Option<&str>,
) -> Result<String, AppError> {
    let requested = entry.filter(|value| !value.is_empty());
    if let Some(raw) = requested
        && let Some(sanitized) = sanitize_relative_path(raw)
    {
        return if clean.iter().any(|(rel, _)| *rel == sanitized) {
            Ok(sanitized)
        } else {
            Err(AppError::Validation(format!(
                "entry \"{raw}\" is not one of the files"
            )))
        };
    }

    if let Some(preferred) = prefer_entry.filter(|value| !value.is_empty())
        && clean.iter().any(|(rel, _)| rel == preferred)
    {
        return Ok(preferred.to_owned());
    }

    if clean.iter().any(|(rel, _)| rel == DEFAULT_BUNDLE_ENTRY) {
        return Ok(DEFAULT_BUNDLE_ENTRY.to_owned());
    }
    clean
        .iter()
        .find(|(rel, _)| rel.ends_with(".html"))
        .map(|(rel, _)| rel.clone())
        .ok_or_else(|| {
            AppError::Validation(
                "no HTML entry found — include index.html or pass an 'entry'".to_owned(),
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reserved_ids_match_the_node_set_and_shape_test() {
        for reserved in RESERVED_ARTIFACT_IDS {
            assert!(is_reserved_artifact_id(reserved), "{reserved}");
        }
        assert!(is_reserved_artifact_id(""));
        assert!(is_reserved_artifact_id("short"));
        assert!(is_reserved_artifact_id("UPPERCASE12"));
        assert!(!is_reserved_artifact_id("abc123def456"));
    }

    #[test]
    fn normalizes_like_node_posix_normalize() {
        for (input, expected) in [
            ("", "."),
            ("a/./b", "a/b"),
            ("a//b", "a/b"),
            ("a/b/", "a/b/"),
            ("./", "./"),
            ("a/..", "."),
            ("a/../", "./"),
            ("a/../..", ".."),
            ("../a", "../a"),
            ("...", "..."),
            ("a/b/../c", "a/c"),
        ] {
            assert_eq!(
                posix_normalize_relative(input),
                expected,
                "normalize({input:?})"
            );
        }
    }

    #[test]
    fn js_trim_follows_ecmascript_whitespace() {
        assert_eq!(js_trim("\u{feff} x \u{feff}"), "x");
        // U+0085 is Unicode White_Space but not ECMAScript WhiteSpace.
        assert_eq!(js_trim("\u{85}x"), "\u{85}x");
    }
}
