//! `artifact-mcp verify-backup`: check a backup directory against its database snapshot.
//!
//! The database defines the required body set. Every current body and every retained history
//! body must be present in the backup, must not depend on a symlink, and must match its recorded
//! digest. Bundle manifests use JavaScript's UTF-16 path order and compact JSON, the same
//! canonical form as [`crate::artifacts::digest::bundle_manifest_digest`]. Included regular
//! preview files must match [`super::PREVIEW_MANIFEST`]. Absent previews and opaque symlinks
//! never become required content.

use std::{
    fmt::{self, Write as _},
    fs::{self, File},
    io::{self, Read as _},
    os::unix::ffi::OsStrExt as _,
    path::{Path, PathBuf},
};

use rusqlite::{Connection, OpenFlags};
use serde_json::Value;
use sha2::{Digest as _, Sha256};

use super::{BackupError, PREVIEW_MANIFEST};
use crate::{
    artifacts::paths::{
        ARTIFACT_DIR_NAME, HISTORY_DIR_NAME, PREVIEW_DIR_NAME, SINGLE_BODY_EXTENSION,
    },
    persistence::db::DATABASE_FILE_NAME,
};

/// Read buffer size for hashing.
const CHUNK: usize = 1024 * 1024;

/// What a backup must contain.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VerifyOptions {
    /// Fail when the backup has no `artifacts.db`. A backup of a data directory without a
    /// database legitimately has none.
    pub database_required: bool,
    /// Fail when the backup has no preview manifest. Every backup this binary writes has one.
    pub preview_manifest_required: bool,
}

/// What a successful verification checked.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VerifySummary {
    /// `SELECT count(*) FROM artifacts`, or `None` when the backup has no database snapshot.
    pub artifacts: Option<u64>,
    /// Current bodies checked.
    pub current_bodies: usize,
    /// Retained history bodies checked.
    pub retained_bodies: usize,
    /// Included preview files checked against the manifest.
    pub previews: usize,
}

impl fmt::Display for VerifySummary {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.artifacts {
            Some(count) => write!(
                formatter,
                "artifacts={count}, current bodies={}, retained bodies={}, previews={}, quick_check ok",
                self.current_bodies, self.retained_bodies, self.previews
            ),
            None => write!(formatter, "no database, previews={}", self.previews),
        }
    }
}

/// Verify the backup directory `root`. Read-only: nothing in `root` is changed.
///
/// # Errors
/// [`BackupError::Coherence`] for missing, unexpected, or mismatched content;
/// [`BackupError::Integrity`] when `PRAGMA quick_check` does not report `ok`.
pub fn verify_backup(root: &Path, options: VerifyOptions) -> Result<VerifySummary, BackupError> {
    let root = std::path::absolute(root)
        .map_err(|error| BackupError::coherence(format!("cannot resolve backup root: {error}")))?;
    if !fs::symlink_metadata(&root).is_ok_and(|metadata| metadata.is_dir()) {
        return Err(BackupError::coherence(
            "backup root must be exactly one existing directory",
        ));
    }
    let previews = verify_previews(&root, options.preview_manifest_required)?;
    let database_path = root.join(DATABASE_FILE_NAME);
    // `os.path.exists`: a dangling symlink counts as absent.
    if fs::metadata(&database_path).is_err() {
        if options.database_required {
            return Err(BackupError::coherence("database snapshot is missing"));
        }
        return Ok(VerifySummary {
            previews,
            ..VerifySummary::default()
        });
    }
    if !fs::symlink_metadata(&database_path).is_ok_and(|metadata| metadata.is_file()) {
        return Err(BackupError::coherence(
            "database snapshot is not a regular file",
        ));
    }
    let connection = Connection::open_with_flags(
        read_only_uri(&database_path),
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_URI
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|error| BackupError::coherence(format!("cannot open database snapshot: {error}")))?;

    let pending: i64 = connection
        .query_row(
            "SELECT count(*) FROM artifact_durability_intents",
            [],
            |row| row.get(0),
        )
        .map_err(|error| {
            BackupError::coherence(format!("durability intent table unavailable: {error}"))
        })?;
    if pending != 0 {
        return Err(BackupError::coherence(format!(
            "{pending} pending durability intent(s)"
        )));
    }

    let (current, retained) = required_bodies(&connection)
        .map_err(|error| BackupError::coherence(format!("artifact tables unavailable: {error}")))?;
    for body in current.iter().chain(&retained) {
        verify_body(&root, body)?;
    }

    let artifacts = integrity(&connection)?;
    Ok(VerifySummary {
        artifacts: Some(artifacts),
        current_bodies: current.len(),
        retained_bodies: retained.len(),
        previews,
    })
}

/// One body the snapshot requires.
struct RequiredBody {
    id: String,
    /// `None` for the current body, otherwise the retained revision.
    revision: Option<i64>,
    bundle: bool,
    expected: Option<String>,
}

fn required_bodies(
    connection: &Connection,
) -> rusqlite::Result<(Vec<RequiredBody>, Vec<RequiredBody>)> {
    let mut statement = connection.prepare("SELECT id, is_bundle, body_sha256 FROM artifacts")?;
    let current = statement
        .query_map([], |row| {
            Ok(RequiredBody {
                id: row.get(0)?,
                revision: None,
                bundle: row.get::<_, Option<i64>>(1)?.unwrap_or(0) != 0,
                expected: row.get(2)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let mut columns = connection.prepare("PRAGMA table_info(artifacts)")?;
    let has_revision = columns
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?
        .iter()
        .any(|name| name == "revision");
    // The current revision's body lives at the current path, so only older revisions (and
    // revisions of deleted artifacts) need a retained body.
    let sql = if has_revision {
        "SELECT r.artifact_id, r.revision, r.is_bundle, r.body_sha256 \
         FROM artifact_revisions r LEFT JOIN artifacts a ON a.id = r.artifact_id \
         WHERE a.id IS NULL OR r.revision < a.revision"
    } else {
        "SELECT artifact_id, revision, is_bundle, body_sha256 FROM artifact_revisions"
    };
    let mut statement = connection.prepare(sql)?;
    let retained = statement
        .query_map([], |row| {
            Ok(RequiredBody {
                id: row.get(0)?,
                revision: Some(row.get(1)?),
                bundle: row.get::<_, Option<i64>>(2)?.unwrap_or(0) != 0,
                expected: row.get(3)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok((current, retained))
}

fn verify_body(root: &Path, body: &RequiredBody) -> Result<(), BackupError> {
    let leaf = match (body.revision, body.bundle) {
        (None, true) => body.id.clone(),
        (None, false) => format!("{}.{SINGLE_BODY_EXTENSION}", body.id),
        (Some(revision), true) => revision.to_string(),
        (Some(revision), false) => format!("{revision}.{SINGLE_BODY_EXTENSION}"),
    };
    let parts: Vec<&str> = match body.revision {
        None => vec![ARTIFACT_DIR_NAME, &leaf],
        Some(_) => vec![ARTIFACT_DIR_NAME, HISTORY_DIR_NAME, &body.id, &leaf],
    };
    let components = contained_components(&parts)
        .ok_or_else(|| BackupError::coherence("required body escapes the backup directory"))?;
    let relative = components.join("/");

    let mut path = root.to_path_buf();
    for component in &components {
        path.push(component);
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(BackupError::coherence(format!(
                    "required body depends on a symlink: {relative}"
                )));
            }
            Ok(_) => {}
            Err(_) => {
                return Err(BackupError::coherence(format!(
                    "required body missing: {relative}"
                )));
            }
        }
    }

    let actual = body_digest(&path, body.bundle)?;
    if let Some(expected) = body.expected.as_deref().filter(|value| !value.is_empty())
        && actual != expected
    {
        return Err(BackupError::coherence(format!(
            "digest mismatch: {relative}"
        )));
    }
    Ok(())
}

/// Join `parts` (each may contain `/`) into normalized components below the backup root, or
/// `None` when they leave it. `.` and empty segments are dropped and `..` pops, as
/// `os.path.relpath` would resolve them; an absolute part is refused.
fn contained_components<'a>(parts: &[&'a str]) -> Option<Vec<&'a str>> {
    let mut components: Vec<&str> = Vec::new();
    for part in parts {
        if part.starts_with('/') || part.contains('\0') {
            return None;
        }
        for segment in part.split('/') {
            match segment {
                "" | "." => {}
                ".." => {
                    components.pop()?;
                }
                other => components.push(other),
            }
        }
    }
    (!components.is_empty()).then_some(components)
}

fn body_digest(path: &Path, bundle: bool) -> Result<String, BackupError> {
    let metadata = fs::symlink_metadata(path).map_err(|error| io_failure(path, &error))?;
    let kind = metadata.file_type();
    if kind.is_symlink() || !(kind.is_file() || kind.is_dir()) {
        return Err(BackupError::coherence(format!(
            "required body is not a regular file/directory: {}",
            path.display()
        )));
    }
    if !bundle {
        if !kind.is_file() {
            return Err(BackupError::coherence(format!(
                "required file body missing or invalid: {}",
                path.display()
            )));
        }
        return sha256_file(path).map_err(|error| io_failure(path, &error));
    }
    if !kind.is_dir() {
        return Err(BackupError::coherence(format!(
            "required bundle directory missing or invalid: {}",
            path.display()
        )));
    }
    bundle_digest(path)
}

/// The canonical digest of a bundle directory: SHA-256 of the compact JSON array of
/// `[relative_path, sha256(content)]` pairs, sorted by UTF-16 code units. Rejects symlinks,
/// special files, and names that are not UTF-8.
///
/// # Errors
/// [`BackupError::Coherence`] for an entry a bundle may not contain or a read failure.
pub fn bundle_digest(directory: &Path) -> Result<String, BackupError> {
    let mut entries = Vec::new();
    collect_bundle(directory, "", &mut entries)?;
    entries.sort_by(|left: &(String, String), right| {
        left.0.encode_utf16().cmp(right.0.encode_utf16())
    });
    let manifest = serde_json::to_string(&entries).map_err(|error| {
        BackupError::coherence(format!("cannot encode bundle manifest: {error}"))
    })?;
    Ok(hex::encode(Sha256::digest(manifest.as_bytes())))
}

fn collect_bundle(
    directory: &Path,
    prefix: &str,
    entries: &mut Vec<(String, String)>,
) -> Result<(), BackupError> {
    let mut names = Vec::new();
    for entry in fs::read_dir(directory).map_err(|error| io_failure(directory, &error))? {
        let name = entry
            .map_err(|error| io_failure(directory, &error))?
            .file_name();
        let Some(name) = name.to_str() else {
            return Err(BackupError::coherence(format!(
                "bundle contains a name that is not UTF-8: {}",
                join_relative(prefix, &name.to_string_lossy())
            )));
        };
        names.push(name.to_owned());
    }
    names.sort_by(|left, right| left.encode_utf16().cmp(right.encode_utf16()));
    for name in names {
        let relative = join_relative(prefix, &name);
        let full = directory.join(&name);
        let metadata = fs::symlink_metadata(&full).map_err(|error| io_failure(&full, &error))?;
        let kind = metadata.file_type();
        if kind.is_symlink() {
            return Err(BackupError::coherence(format!(
                "bundle contains symlink: {relative}"
            )));
        } else if kind.is_dir() {
            collect_bundle(&full, &relative, entries)?;
        } else if kind.is_file() {
            let digest = sha256_file(&full).map_err(|error| io_failure(&full, &error))?;
            entries.push((relative, digest));
        } else {
            return Err(BackupError::coherence(format!(
                "bundle contains unsupported entry: {relative}"
            )));
        }
    }
    Ok(())
}

fn join_relative(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_owned()
    } else {
        format!("{prefix}/{name}")
    }
}

/// Check every included regular preview against the manifest. Returns how many were checked.
fn verify_previews(root: &Path, required: bool) -> Result<usize, BackupError> {
    let manifest_path = root.join(PREVIEW_MANIFEST);
    match fs::symlink_metadata(&manifest_path) {
        Err(_) if required => {
            return Err(BackupError::coherence(
                "optional preview manifest is missing",
            ));
        }
        Err(_) => return Ok(0),
        Ok(metadata) if !metadata.is_file() => {
            return Err(BackupError::coherence(
                "optional preview manifest is not a regular file",
            ));
        }
        Ok(_) => {}
    }
    let invalid = || BackupError::coherence("optional preview manifest is invalid");
    let bytes = fs::read(&manifest_path).map_err(|error| io_failure(&manifest_path, &error))?;
    let manifest: Value = serde_json::from_slice(&bytes).map_err(|_| invalid())?;
    let object = manifest.as_object().ok_or_else(invalid)?;
    if object.get("version").and_then(Value::as_f64) != Some(1.0) {
        return Err(invalid());
    }
    let files = object
        .get("files")
        .and_then(Value::as_object)
        .ok_or_else(invalid)?;

    let mut checked = 0;
    for (relative, path) in preview_files(&root.join(PREVIEW_DIR_NAME))? {
        let actual = sha256_file(&path).map_err(|error| io_failure(&path, &error))?;
        if files.get(&relative).and_then(Value::as_str) != Some(actual.as_str()) {
            return Err(BackupError::coherence(format!(
                "optional preview digest mismatch: {relative}"
            )));
        }
        checked += 1;
    }
    Ok(checked)
}

/// Regular files below a preview directory, keyed by `/`-joined relative path. A missing or
/// symlinked preview directory has none; symlinks inside it are skipped.
fn preview_files(directory: &Path) -> Result<Vec<(String, PathBuf)>, BackupError> {
    let mut files = Vec::new();
    match fs::symlink_metadata(directory) {
        Err(_) => return Ok(files),
        Ok(metadata) if metadata.file_type().is_symlink() => return Ok(files),
        Ok(metadata) if !metadata.is_dir() => {
            return Err(BackupError::coherence(
                "preview cache must be a directory when included",
            ));
        }
        Ok(_) => {}
    }
    collect_previews(directory, "", &mut files)?;
    Ok(files)
}

fn collect_previews(
    directory: &Path,
    prefix: &str,
    files: &mut Vec<(String, PathBuf)>,
) -> Result<(), BackupError> {
    let mut entries = Vec::new();
    for entry in fs::read_dir(directory).map_err(|error| io_failure(directory, &error))? {
        let entry = entry.map_err(|error| io_failure(directory, &error))?;
        entries.push((entry.file_name(), entry.path()));
    }
    entries.sort();
    for (name, path) in entries {
        let relative = join_relative(prefix, &name.to_string_lossy());
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            continue;
        };
        let kind = metadata.file_type();
        if kind.is_symlink() {
            continue;
        } else if kind.is_dir() {
            collect_previews(&path, &relative, files)?;
        } else if kind.is_file() {
            files.push((relative, path));
        } else {
            return Err(BackupError::coherence(
                "preview cache contains an unsupported entry",
            ));
        }
    }
    Ok(())
}

/// `PRAGMA quick_check`, then the artifact count.
fn integrity(connection: &Connection) -> Result<u64, BackupError> {
    let mut statement = connection
        .prepare("PRAGMA quick_check")
        .map_err(|error| BackupError::Integrity(error.to_string()))?;
    let lines = statement
        .query_map([], |row| row.get::<_, String>(0))
        .and_then(Iterator::collect::<rusqlite::Result<Vec<_>>>)
        .map_err(|error| BackupError::Integrity(error.to_string()))?;
    if lines.first().map(String::as_str) != Some("ok") {
        return Err(BackupError::Integrity(lines.join("\n")));
    }
    let count: i64 = connection
        .query_row("SELECT count(*) FROM artifacts", [], |row| row.get(0))
        .map_err(|error| BackupError::Integrity(error.to_string()))?;
    Ok(u64::try_from(count).unwrap_or(0))
}

/// `file:<percent-encoded absolute path>?mode=ro`, so `%`, `#`, `?`, and quotes in the path stay
/// literal. Unreserved bytes and `/` are kept, as Python's `quote(path, safe="/")` keeps them.
fn read_only_uri(path: &Path) -> String {
    let mut uri = String::from("file:");
    for &byte in path.as_os_str().as_bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'-' | b'.' | b'_' | b'~') {
            uri.push(char::from(byte));
        } else {
            let _ = write!(uri, "%{byte:02X}");
        }
    }
    uri.push_str("?mode=ro");
    uri
}

fn sha256_file(path: &Path) -> io::Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; CHUNK];
    loop {
        match file.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => hasher.update(&buffer[..read]),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(hex::encode(hasher.finalize()))
}

fn io_failure(path: &Path, error: &io::Error) -> BackupError {
    BackupError::coherence(format!("{error}: {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::artifacts::digest::{body_digest_at_path, bundle_manifest_digest};

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("verify-unit-{}", nanoid::nanoid!(12)));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn bundle_digest_matches_the_server_digest() {
        let temp = TempDir::new();
        let files: [(&str, &[u8]); 7] = [
            ("index.html", b"index"),
            ("z.html", b"z"),
            ("assets/site.css", b"css"),
            ("assets/\u{e000}.txt", b"private use"),
            ("assets/\u{10000}.txt", b"astral"),
            ("quote\"back\\slash.txt", b"escaped"),
            ("caf\u{e9}/tab\tname.js", b"control"),
        ];
        for (name, content) in files {
            let path = temp.0.join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, content).unwrap();
        }
        let expected = bundle_manifest_digest(&files);
        assert_eq!(bundle_digest(&temp.0).unwrap(), expected);
        assert_eq!(body_digest_at_path(&temp.0, true).unwrap(), expected);
    }

    #[test]
    fn bundle_digest_rejects_symlinks() {
        let temp = TempDir::new();
        fs::write(temp.0.join("index.html"), b"x").unwrap();
        std::os::unix::fs::symlink("index.html", temp.0.join("link.html")).unwrap();
        let error = bundle_digest(&temp.0).unwrap_err().to_string();
        assert_eq!(
            error,
            "coherence check failed: bundle contains symlink: link.html"
        );
    }

    #[test]
    fn required_paths_stay_inside_the_backup() {
        assert_eq!(
            contained_components(&["artifacts", "abc.html"]),
            Some(vec!["artifacts", "abc.html"])
        );
        assert_eq!(
            contained_components(&["artifacts", "a/./b"]),
            Some(vec!["artifacts", "a", "b"])
        );
        assert_eq!(
            contained_components(&["artifacts", "../x.html"]),
            Some(vec!["x.html"])
        );
        assert_eq!(contained_components(&["artifacts", "../../x.html"]), None);
        assert_eq!(contained_components(&["artifacts", "/etc/passwd"]), None);
        assert_eq!(contained_components(&["artifacts", ".."]), None);
    }

    #[test]
    fn read_only_uri_escapes_uri_punctuation() {
        assert_eq!(
            read_only_uri(Path::new("/tmp/backups ' % # ?/artifacts.db")),
            "file:/tmp/backups%20%27%20%25%20%23%20%3F/artifacts.db?mode=ro"
        );
    }
}
