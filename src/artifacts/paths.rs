//! Artifact, history, and preview paths.
//!
//! The on-disk layout is frozen by the rebuild blueprint §D2 and must not change; bodies do
//! not move during cutover, and Node and Rust have to alternate against the same directory.
//!
//! ```text
//! /data/artifacts/<id>.html
//! /data/artifacts/<id>/<bundle-relative-path>
//! /data/artifacts/.history/<id>/<revision>.html
//! /data/artifacts/.history/<id>/<revision>/<bundle-relative-path>
//! /data/artifacts/.<id>.staging-<random>
//! /data/artifacts/.<id>.trash-<random>
//! /data/previews/<id>/<body_sha256>.png
//! ```
//!
//! Node oracle, function by function:
//!
//! | Rust | Node |
//! |---|---|
//! | [`artifact_dir`] | `path.join(dataDir, "artifacts")` |
//! | [`single_body_path`] | `filePath(id)` |
//! | [`bundle_dir`] | `bundleDir(id)` |
//! | [`transient_path`] | `transientPath(id, kind)` |
//! | [`history_root`] / [`history_dir`] | `HISTORY_DIR`, `historyDir(id)` |
//! | [`history_body_path`] | `historyBodyPath(id, revision, isBundle)` |
//! | [`preview_dir`] | `path.join(dataDir, "previews")` |
//! | [`preview_artifact_dir`] | `artifactDir(id)` |
//! | [`thumbnail_path`] | `thumbnailPath(id, digest)` |
//!
//! Every constructor takes an already-validated [`SafeArtifactId`] / [`BodyDigest`], so no caller-supplied string ever reaches a path segment unchecked. That is stricter than Node, which only guards the preview paths; it cannot change behaviour for real artifacts because every generated id satisfies `/^[0-9a-z]{6,24}$/`.

use std::path::{Path, PathBuf};

pub use super::validation::{BodyDigest, SafeArtifactId};
pub use crate::persistence::db::{ARTIFACT_DIR_NAME, artifact_dir};

/// `const HISTORY_DIR = ".history"`
pub const HISTORY_DIR_NAME: &str = ".history";

/// `path.join(dataDir, "previews")`
pub const PREVIEW_DIR_NAME: &str = "previews";

/// Extension of a single-file body and of a single-file history snapshot.
pub const SINGLE_BODY_EXTENSION: &str = "html";

/// Extension of a persisted thumbnail.
pub const THUMBNAIL_EXTENSION: &str = "png";

/// The two transient path kinds in `transientPath(id, kind)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum TransientKind {
    /// A body written before the metadata commit, swapped in afterwards.
    Staging,
    /// A body moved aside before a delete, discarded once the row is gone.
    Trash,
}

impl TransientKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Staging => "staging",
            Self::Trash => "trash",
        }
    }
}

// ---------------------------------------------------------------------------
// Live bodies
// ---------------------------------------------------------------------------

/// ``filePath(id) = path.join(artifactDir, `<id>.html`)``
#[must_use]
pub fn single_body_path(artifact_dir: &Path, id: &SafeArtifactId) -> PathBuf {
    artifact_dir.join(format!("{}.{SINGLE_BODY_EXTENSION}", id.as_str()))
}

/// `bundleDir(id) = path.join(artifactDir, id)`
#[must_use]
pub fn bundle_dir(artifact_dir: &Path, id: &SafeArtifactId) -> PathBuf {
    artifact_dir.join(id.as_str())
}

/// The live body path for an artifact of either shape.
#[must_use]
pub fn body_path(artifact_dir: &Path, id: &SafeArtifactId, is_bundle: bool) -> PathBuf {
    if is_bundle {
        bundle_dir(artifact_dir, id)
    } else {
        single_body_path(artifact_dir, id)
    }
}

// ---------------------------------------------------------------------------
// Transient bodies
// ---------------------------------------------------------------------------

/// ``transientPath(id, kind) = path.join(artifactDir, `.${id}.${kind}-${generateId()}`)``
///
/// `token` is a caller-supplied 12-character nanoid over the artifact alphabet.
/// Returns `None` for a token outside that format, so the suffix cannot become an injection point.
#[must_use]
pub fn transient_path(
    artifact_dir: &Path,
    id: &SafeArtifactId,
    kind: TransientKind,
    token: &str,
) -> Option<PathBuf> {
    let token = SafeArtifactId::parse(token)?;
    Some(artifact_dir.join(format!(
        ".{}.{}-{}",
        id.as_str(),
        kind.as_str(),
        token.as_str()
    )))
}

/// `name.startsWith(".") && (name.includes(".staging-") || name.includes(".trash-"))`
#[must_use]
pub fn is_transient_name(name: &str) -> bool {
    name.starts_with('.') && (name.contains(".staging-") || name.contains(".trash-"))
}

/// ``name.match(/^\.([0-9a-z]{6,24})\.(?:staging|trash)-/)``
///
/// Recovers the owning artifact id from a transient directory entry so the storage audit can
/// decide whether the interrupted body still belongs at the final path.
#[must_use]
pub fn transient_name_artifact_id(name: &str) -> Option<SafeArtifactId> {
    let rest = name.strip_prefix('.')?;
    let (id, suffix) = rest.split_once('.')?;
    let tail = suffix
        .strip_prefix(TransientKind::Staging.as_str())
        .or_else(|| suffix.strip_prefix(TransientKind::Trash.as_str()))?;
    tail.strip_prefix('-')?;
    SafeArtifactId::parse(id)
}

// ---------------------------------------------------------------------------
// Version history
// ---------------------------------------------------------------------------

/// `path.join(artifactDir, HISTORY_DIR)`
#[must_use]
pub fn history_root(artifact_dir: &Path) -> PathBuf {
    artifact_dir.join(HISTORY_DIR_NAME)
}

/// `historyDir(id) = path.join(artifactDir, HISTORY_DIR, id)`
#[must_use]
pub fn history_dir(artifact_dir: &Path, id: &SafeArtifactId) -> PathBuf {
    history_root(artifact_dir).join(id.as_str())
}

/// ``historyBodyPath(id, revision, isBundle) =
/// path.join(historyDir(id), isBundle ? String(revision) : `${revision}.html`)``
#[must_use]
pub fn history_body_path(
    artifact_dir: &Path,
    id: &SafeArtifactId,
    revision: u64,
    is_bundle: bool,
) -> PathBuf {
    let dir = history_dir(artifact_dir, id);
    if is_bundle {
        dir.join(revision.to_string())
    } else {
        dir.join(format!("{revision}.{SINGLE_BODY_EXTENSION}"))
    }
}

/// Temporary copy owned by a metadata-only update while publishing immutable history.
///
/// Keep this derivation in one place: Rust intentionally replaces the history body's extension,
/// so `1.html` becomes `1.snapshot-tmp` while a bundle directory `1` does the same.
#[must_use]
pub fn history_snapshot_temp_path(
    artifact_dir: &Path,
    id: &SafeArtifactId,
    revision: u64,
    is_bundle: bool,
) -> PathBuf {
    history_body_path(artifact_dir, id, revision, is_bundle).with_extension("snapshot-tmp")
}

// ---------------------------------------------------------------------------
// Previews
// ---------------------------------------------------------------------------

/// `const previewDir = path.join(dataDir, "previews")`
#[must_use]
pub fn preview_dir(data_dir: &Path) -> PathBuf {
    data_dir.join(PREVIEW_DIR_NAME)
}

/// `artifactDir(id)` in the thumbnail store
#[must_use]
pub fn preview_artifact_dir(data_dir: &Path, id: &SafeArtifactId) -> PathBuf {
    preview_dir(data_dir).join(id.as_str())
}

/// ``thumbnailPath(id, digest) = path.join(artifactDir(id), `${digest}.png`)``
#[must_use]
pub fn thumbnail_path(data_dir: &Path, id: &SafeArtifactId, digest: &BodyDigest) -> PathBuf {
    preview_artifact_dir(data_dir, id).join(format!("{}.{THUMBNAIL_EXTENSION}", digest.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id() -> SafeArtifactId {
        SafeArtifactId::parse("abc123def456").expect("valid id")
    }

    #[test]
    fn builds_the_frozen_layout() {
        let artifacts = Path::new("/data/artifacts");
        let id = id();
        assert_eq!(
            single_body_path(artifacts, &id),
            Path::new("/data/artifacts/abc123def456.html")
        );
        assert_eq!(
            bundle_dir(artifacts, &id),
            Path::new("/data/artifacts/abc123def456")
        );
        assert_eq!(
            history_body_path(artifacts, &id, 7, false),
            Path::new("/data/artifacts/.history/abc123def456/7.html")
        );
        assert_eq!(
            history_body_path(artifacts, &id, 7, true),
            Path::new("/data/artifacts/.history/abc123def456/7")
        );
        assert_eq!(
            transient_path(artifacts, &id, TransientKind::Staging, "qqqqqqqqqqqq"),
            Some(PathBuf::from(
                "/data/artifacts/.abc123def456.staging-qqqqqqqqqqqq"
            ))
        );
    }

    #[test]
    fn rejects_an_unusable_transient_token() {
        assert!(
            transient_path(
                Path::new("/data/artifacts"),
                &id(),
                TransientKind::Trash,
                "../../etc"
            )
            .is_none()
        );
    }
}
