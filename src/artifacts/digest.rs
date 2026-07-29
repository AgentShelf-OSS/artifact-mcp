//! Owned by U07 (terra) — canonical body and bundle digests.
//!
//! Node oracle:
//!
//! ```text
//! function sha256(content) {
//!   return createHash("sha256").update(content).digest("hex");
//! }
//! function bundleManifestDigest(entries) {
//!   const manifest = entries
//!     .map(([rel, content]) => [rel, sha256(content)])
//!     .sort(([a], [b]) => a < b ? -1 : a > b ? 1 : 0);
//!   return sha256(JSON.stringify(manifest));
//! }
//! ```
//! — [lib/store.js:77-86]
//!
//! and `bodyDigestAtPath` / `bodyDigestOnDisk` — [lib/store.js:649-673].
//!
//! This value is load bearing beyond display: it addresses the preview file
//! (`/data/previews/<id>/<body_sha256>.png`, [lib/thumbnails.js:80-84]), it is the durable
//! commit marker crash recovery compares against ([lib/store.js:705]), and the v17
//! `artifact-body-digest` migration backfills it ([lib/store.js:755-770]). It must stay byte
//! identical to Node's.

use std::cmp::Ordering;
use std::io;
use std::path::Path;

use serde_json::Value;
use sha2::{Digest, Sha256};

const HEX: [u8; 16] = *b"0123456789abcdef";

/// `createHash("sha256").update(content).digest("hex")` — [lib/store.js:77-79]
///
/// Node hashes a JS string as UTF-8, so `sha256_hex(text.as_bytes())` is the same digest.
#[must_use]
pub fn sha256_hex(content: &[u8]) -> String {
    let digest = Sha256::digest(content);
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        out.push(char::from(HEX[usize::from(byte >> 4)]));
        out.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    out
}

/// The digest of a single-file body. [lib/store.js:209], [lib/store.js:651]
#[must_use]
pub fn single_body_digest(content: &[u8]) -> String {
    sha256_hex(content)
}

/// JavaScript's `a < b` / `a > b` string comparison, i.e. UTF-16 code-unit order.
///
/// Not the same as Rust's byte-wise `Ord` for astral characters: `"\u{10000}"` sorts *before*
/// `"\u{e000}"` in UTF-16 (its lead surrogate is 0xD800) but after it in UTF-8. The manifest
/// sort key is a file path, which can legitimately contain such characters.
fn utf16_cmp(left: &str, right: &str) -> Ordering {
    left.encode_utf16().cmp(right.encode_utf16())
}

/// `bundleManifestDigest(entries)` — [lib/store.js:81-86]
///
/// Order independent by construction: the caller may pass entries in filesystem enumeration
/// order (as the storage audit does) or in publish order, and the sort makes the result the
/// same. `Array.prototype.sort` is stable in V8 and the comparator returns 0 for equal keys,
/// so duplicate relative paths keep their input order — matched here by `sort_by`, which is
/// also stable.
///
/// The manifest is serialized exactly as `JSON.stringify` would: a compact array of
/// two-element arrays, `"`/`\` and control characters escaped, non-ASCII emitted literally.
#[must_use]
pub fn bundle_manifest_digest<P, C>(entries: &[(P, C)]) -> String
where
    P: AsRef<str>,
    C: AsRef<[u8]>,
{
    let mut manifest: Vec<(String, String)> = entries
        .iter()
        .map(|(rel, content)| (rel.as_ref().to_owned(), sha256_hex(content.as_ref())))
        .collect();
    manifest.sort_by(|left, right| utf16_cmp(&left.0, &right.0));

    let json = Value::Array(
        manifest
            .into_iter()
            .map(|(rel, digest)| Value::Array(vec![Value::String(rel), Value::String(digest)]))
            .collect(),
    )
    // `Display for Value` is infallible and emits the same compact form as `JSON.stringify`.
    .to_string();
    sha256_hex(json.as_bytes())
}

/// `bodyDigestAtPath(target, isBundle)` — [lib/store.js:649-669]
///
/// Returns `None` where Node's `try { … } catch { return null; }` would: a missing body, an
/// unreadable file, or `readdirSync` on something that is not a directory.
#[must_use]
pub fn body_digest_at_path(target: &Path, is_bundle: bool) -> Option<String> {
    if !is_bundle {
        return std::fs::read(target).ok().map(|bytes| sha256_hex(&bytes));
    }
    let mut entries: Vec<(String, Vec<u8>)> = Vec::new();
    collect_bundle_entries(target, target, &mut entries).ok()?;
    Some(bundle_manifest_digest(&entries))
}

/// The `walk` closure inside `bodyDigestAtPath`: every regular file under `root`, keyed by its
/// `/`-joined path relative to `root`. Directories contribute nothing on their own.
/// [lib/store.js:653-664]
fn collect_bundle_entries(
    root: &Path,
    dir: &Path,
    entries: &mut Vec<(String, Vec<u8>)>,
) -> io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let full = entry.path();
        // `statSync` follows symlinks, and so does `fs::metadata`.
        if std::fs::metadata(&full)?.is_dir() {
            collect_bundle_entries(root, &full, entries)?;
        } else {
            let rel = full
                .strip_prefix(root)
                .map_err(|_| io::Error::other("bundle entry escaped its root"))?;
            entries.push((relative_key(rel), std::fs::read(&full)?));
        }
    }
    Ok(())
}

/// `path.relative(base, full).split(path.sep).join("/")` — [lib/store.js:660]
fn relative_key(rel: &Path) -> String {
    rel.components()
        .map(|component| component.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashes_utf8_bytes_like_node() {
        // `createHash("sha256").update("").digest("hex")`
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        // `createHash("sha256").update("abc").digest("hex")`
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn bundle_digest_ignores_enumeration_order() {
        let forward = [
            ("a.html", b"one".as_slice()),
            ("b/c.css", b"two".as_slice()),
        ];
        let reversed = [
            ("b/c.css", b"two".as_slice()),
            ("a.html", b"one".as_slice()),
        ];
        assert_eq!(
            bundle_manifest_digest(&forward),
            bundle_manifest_digest(&reversed)
        );
    }

    #[test]
    fn bundle_digest_separates_path_from_content() {
        let one = [("a", b"x".as_slice()), ("b", b"y".as_slice())];
        let two = [("a", b"y".as_slice()), ("b", b"x".as_slice())];
        assert_ne!(bundle_manifest_digest(&one), bundle_manifest_digest(&two));
    }

    #[test]
    fn manifest_sort_uses_utf16_code_units() {
        assert_eq!(utf16_cmp("\u{10000}", "\u{e000}"), Ordering::Less);
        assert_eq!("\u{10000}".cmp("\u{e000}"), Ordering::Greater);
    }
}
