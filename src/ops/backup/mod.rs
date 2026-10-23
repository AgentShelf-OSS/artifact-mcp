//! `artifact-mcp backup`: a consistent, verified backup of a data directory.
//!
//! Why not `cp data/artifacts.db`: the database runs in WAL mode, so committed data lives in
//! `artifacts.db-wal` until a checkpoint. Copying the `.db` alone yields a stale or torn backup.
//! `VACUUM INTO` asks SQLite itself to write a consistent snapshot to a new file inside one read
//! transaction. No writer stop, no `-wal`/`-shm` to carry, and the result is already compacted.
//! (<https://sqlite.org/lang_vacuum.html#vacuuminto>)
//!
//! The order of a run is the guarantee:
//!
//! 1. The database snapshot is taken **first**. It is the recovery point.
//! 2. Bodies, retained history, transient staging/trash evidence, and optional previews are
//!    copied only after that cut.
//! 3. The snapshot is verified against the copied files ([`verify_backup`]): no pending
//!    durability intent, no missing current or retained body, canonical bundle manifests and
//!    SHA-256 digests, every included preview equal to the bytes read from the source, and
//!    `PRAGMA quick_check`.
//! 4. Copied files and directories are fsynced, the staging directory is renamed to its final
//!    name, and the destination directory is synced.
//! 5. Older completed backups beyond `KEEP` are pruned ([`prune_backups`]).
//!
//! A concurrent lifecycle mutation can therefore make the verification fail, but it can never
//! publish a database row whose required body is absent from the completed backup. Any failure
//! or interruption removes the staging directory, so an incomplete run is never mistaken for a
//! good one.

mod copy;
mod verify;

use std::{
    fmt,
    fs::{self, DirBuilder, File},
    io,
    os::unix::fs::{DirBuilderExt as _, MetadataExt as _, PermissionsExt as _},
    path::{Path, PathBuf},
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use rusqlite::{Connection, InterruptHandle, OpenFlags};
use time::OffsetDateTime;

pub use verify::{VerifyOptions, VerifySummary, bundle_digest, verify_backup};

use crate::persistence::db::DATABASE_FILE_NAME;

/// Default number of completed backups kept by [`prune_backups`].
pub const DEFAULT_KEEP: usize = 14;
/// SHA-256 digests of the preview files included in a backup, keyed by path below `previews/`.
pub const PREVIEW_MANIFEST: &str = "preview-sha256.json";
/// Name prefix of a completed backup directory.
pub const BACKUP_PREFIX: &str = "backup-";
/// Name prefix of a staging directory that has not passed verification.
pub const STAGING_PREFIX: &str = ".incomplete-";
/// Mode of a new staging directory, and so of a completed backup (as `mktemp -d`).
const STAGE_MODE: u32 = 0o700;
/// SQLite busy timeout for the snapshot read transaction.
const SNAPSHOT_BUSY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// A failed backup or verification. The CLI prints each as `backup: <message>`.
#[derive(Debug, thiserror::Error)]
pub enum BackupError {
    /// `KEEP` is not a positive integer.
    #[error("KEEP must be a positive integer")]
    InvalidKeep,
    /// The data directory does not exist or is not a directory.
    #[error("DATA_DIR '{}' not found", .0.display())]
    DataDirNotFound(PathBuf),
    /// `VACUUM INTO` finished without producing the snapshot file.
    #[error("database snapshot was not created")]
    SnapshotMissing,
    /// The staged copy does not match its database snapshot.
    #[error("coherence check failed: {0}")]
    Coherence(String),
    /// `PRAGMA quick_check` did not report `ok`.
    #[error("integrity check FAILED: {0}")]
    Integrity(String),
    /// A signal or the caller cancelled the run.
    #[error("interrupted; incomplete staging removed")]
    Interrupted,
    /// A file system operation failed.
    #[error("{action}: {source}")]
    Io {
        action: String,
        #[source]
        source: io::Error,
    },
    /// A database operation failed.
    #[error("{action}: {source}")]
    Database {
        action: String,
        #[source]
        source: rusqlite::Error,
    },
}

impl BackupError {
    fn io(action: impl Into<String>, source: io::Error) -> Self {
        Self::Io {
            action: action.into(),
            source,
        }
    }

    fn coherence(message: impl Into<String>) -> Self {
        Self::Coherence(message.into())
    }
}

/// What to back up, where, and how many completed backups to keep.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BackupOptions {
    /// The server's `DATA_DIR`: `artifacts.db`, `artifacts/`, and `previews/`.
    pub data_dir: PathBuf,
    /// The directory that holds `backup-*` directories.
    pub dest_root: PathBuf,
    /// Completed backups to keep, at least 1.
    pub keep: usize,
}

/// Parse `KEEP` as the former script did: decimal digits, optionally zero-padded, not zero.
/// A value too large for `usize` keeps everything.
///
/// # Errors
/// [`BackupError::InvalidKeep`] for anything else.
pub fn parse_keep(raw: &str) -> Result<usize, BackupError> {
    let digits = raw.trim_start_matches('0');
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(BackupError::InvalidKeep);
    }
    Ok(digits.parse().unwrap_or(usize::MAX))
}

/// Cancellation shared between a running backup and a signal handler.
///
/// Cancelling sets a flag that the run checks between files and interrupts an in-progress
/// SQLite snapshot.
#[derive(Default)]
pub struct Cancel {
    requested: AtomicBool,
    sqlite: Mutex<Option<InterruptHandle>>,
}

impl fmt::Debug for Cancel {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Cancel")
            .field("requested", &self.is_cancelled())
            .finish_non_exhaustive()
    }
}

impl Cancel {
    /// Ask the run to stop and remove its staging directory.
    pub fn cancel(&self) {
        self.requested.store(true, Ordering::SeqCst);
        if let Some(handle) = self.lock().as_ref() {
            handle.interrupt();
        }
    }

    /// Whether [`Cancel::cancel`] was called.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.requested.load(Ordering::SeqCst)
    }

    /// `Err(Interrupted)` once cancelled.
    ///
    /// # Errors
    /// [`BackupError::Interrupted`] after [`Cancel::cancel`].
    pub fn check(&self) -> Result<(), BackupError> {
        if self.is_cancelled() {
            Err(BackupError::Interrupted)
        } else {
            Ok(())
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<InterruptHandle>> {
        self.sqlite
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn attach(&self, connection: &Connection) -> Result<(), BackupError> {
        *self.lock() = Some(connection.get_interrupt_handle());
        // Checked after the handle is visible, so a concurrent cancel either sees the handle or
        // is seen here.
        self.check()
    }

    fn detach(&self) {
        *self.lock() = None;
    }
}

/// A point in a run where an observer may act. Tests use these to interleave mutations with the
/// run deterministically.
#[derive(Clone, Copy, Debug)]
pub enum Checkpoint<'a> {
    /// Before the database snapshot.
    BeforeSnapshot,
    /// After the database snapshot, before any file is copied.
    AfterSnapshot { stage: &'a Path },
    /// After the artifact bodies are copied, before the optional previews.
    BeforePreviewCopy { stage: &'a Path },
    /// After the previews are copied, before verification.
    AfterPreviewCopy { stage: &'a Path },
    /// After verification and fsync, before the atomic rename.
    BeforePublish {
        stage: &'a Path,
        final_path: &'a Path,
    },
}

/// Receives checkpoints and warnings from a run.
pub trait BackupObserver {
    /// Called at each [`Checkpoint`]. An error stops the run and removes the staging directory.
    ///
    /// # Errors
    /// Whatever the observer wants the run to fail with.
    fn reached(&mut self, point: Checkpoint<'_>) -> Result<(), BackupError> {
        let _ = point;
        Ok(())
    }

    /// A non-fatal problem, such as an incomplete optional preview copy.
    fn warning(&mut self, message: &str) {
        eprintln!("backup: {message}");
    }
}

impl<F> BackupObserver for F
where
    F: FnMut(Checkpoint<'_>) -> Result<(), BackupError>,
{
    fn reached(&mut self, point: Checkpoint<'_>) -> Result<(), BackupError> {
        self(point)
    }
}

/// An observer that only prints warnings.
#[derive(Clone, Copy, Debug, Default)]
pub struct WarningsToStderr;

impl BackupObserver for WarningsToStderr {}

/// A completed, verified, published backup.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BackupReport {
    /// The final backup directory, below the destination root as given.
    pub path: PathBuf,
    /// Allocated size of the backup in bytes, as `du` counts it.
    pub disk_usage: u64,
    /// What verification checked.
    pub summary: VerifySummary,
}

impl fmt::Display for BackupReport {
    /// `backup: <path> (<size>, artifacts=<n>, quick_check ok)`, the former script's line.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let count = match self.summary.artifacts {
            Some(count) => format!("artifacts={count}"),
            None => "artifacts=0 (no database)".to_owned(),
        };
        write!(
            formatter,
            "backup: {} ({}, {count}, quick_check ok)",
            self.path.display(),
            human_size(self.disk_usage)
        )
    }
}

/// Removes the staging directory unless the run published it.
struct Stage {
    path: PathBuf,
    armed: bool,
}

impl Drop for Stage {
    fn drop(&mut self) {
        if self.armed {
            let _ = remove_tree(&self.path);
        }
    }
}

/// Create, verify, and publish one backup. Does not prune; see [`prune_backups`].
///
/// # Errors
/// Any [`BackupError`]. On error, no completed backup is published and the staging directory is
/// removed.
pub fn run_backup(
    options: &BackupOptions,
    cancel: &Cancel,
    observer: &mut dyn BackupObserver,
) -> Result<BackupReport, BackupError> {
    if options.keep == 0 {
        return Err(BackupError::InvalidKeep);
    }
    let data_dir = &options.data_dir;
    if !fs::metadata(data_dir).is_ok_and(|metadata| metadata.is_dir()) {
        return Err(BackupError::DataDirNotFound(data_dir.clone()));
    }
    let stamp = stamp(OffsetDateTime::now_utc());
    create_dest_root(&options.dest_root)?;
    let (stage, suffix) = create_stage(&options.dest_root, &stamp)?;
    let final_path = options
        .dest_root
        .join(format!("{BACKUP_PREFIX}{stamp}-{suffix}"));
    let stage_dir = stage.path.clone();

    // 1. Consistent database snapshot: the recovery point for every file copied below.
    observer.reached(Checkpoint::BeforeSnapshot)?;
    cancel.check()?;
    let source_db = data_dir.join(DATABASE_FILE_NAME);
    let has_database = fs::metadata(&source_db).is_ok_and(|metadata| metadata.is_file());
    if has_database {
        let snapshot = stage_dir.join(DATABASE_FILE_NAME);
        snapshot_database(&source_db, &snapshot, cancel)?;
        if !fs::symlink_metadata(&snapshot).is_ok_and(|metadata| metadata.is_file()) {
            return Err(BackupError::SnapshotMissing);
        }
    }
    observer.reached(Checkpoint::AfterSnapshot { stage: &stage_dir })?;
    cancel.check()?;

    // 2. Copy after the database cut, keeping hidden staging, trash, and history evidence.
    copy::copy_artifacts(data_dir, &stage_dir, cancel, observer)?;
    observer.reached(Checkpoint::BeforePreviewCopy { stage: &stage_dir })?;
    cancel.check()?;
    copy::copy_previews(data_dir, &stage_dir, cancel, observer)?;
    observer.reached(Checkpoint::AfterPreviewCopy { stage: &stage_dir })?;
    cancel.check()?;

    // 3. Verify before publishing. An unverified backup is a guess.
    let summary = verify_backup(
        &stage_dir,
        VerifyOptions {
            database_required: has_database,
            preview_manifest_required: true,
        },
    )?;
    cancel.check()?;
    copy::sync_tree(&stage_dir, cancel)?;

    // 4. Publish atomically, then make the new name durable.
    observer.reached(Checkpoint::BeforePublish {
        stage: &stage_dir,
        final_path: &final_path,
    })?;
    cancel.check()?;
    fs::rename(&stage_dir, &final_path).map_err(|error| {
        BackupError::io(
            format!(
                "cannot publish {} as {}",
                stage_dir.display(),
                final_path.display()
            ),
            error,
        )
    })?;
    let mut stage = stage;
    stage.armed = false;
    sync_directory(&options.dest_root)?;
    let disk_usage = disk_usage(&final_path).unwrap_or(0);
    Ok(BackupReport {
        path: final_path,
        disk_usage,
        summary,
    })
}

/// Remove the oldest completed backups so that at most `keep` remain. Names sort by their UTC
/// timestamp. Returns the names removed.
///
/// # Errors
/// [`BackupError::Interrupted`] when cancelled; otherwise the first removal that failed, after
/// trying the rest.
pub fn prune_backups(
    dest_root: &Path,
    keep: usize,
    cancel: &Cancel,
) -> Result<Vec<String>, BackupError> {
    let mut names = Vec::new();
    for entry in fs::read_dir(dest_root)
        .map_err(|error| BackupError::io(format!("cannot list {}", dest_root.display()), error))?
    {
        let entry = entry.map_err(|error| {
            BackupError::io(format!("cannot list {}", dest_root.display()), error)
        })?;
        let name = entry.file_name();
        if name
            .as_encoded_bytes()
            .starts_with(BACKUP_PREFIX.as_bytes())
        {
            names.push(name);
        }
    }
    names.sort();
    let excess = names.len().saturating_sub(keep);
    let mut pruned = Vec::new();
    let mut failure = None;
    for name in names.into_iter().take(excess) {
        cancel.check()?;
        let path = dest_root.join(&name);
        match remove_tree(&path) {
            Ok(()) => pruned.push(name.to_string_lossy().into_owned()),
            Err(error) => {
                failure.get_or_insert_with(|| {
                    BackupError::io(format!("cannot prune {}", path.display()), error)
                });
            }
        }
    }
    match failure {
        Some(error) => Err(error),
        None => Ok(pruned),
    }
}

/// `YYYYMMDD-HHMMSS-NNNNNNNNN` in UTC, so names sort in creation order.
fn stamp(now: OffsetDateTime) -> String {
    format!(
        "{:04}{:02}{:02}-{:02}{:02}{:02}-{:09}",
        now.year(),
        u8::from(now.month()),
        now.day(),
        now.hour(),
        now.minute(),
        now.second(),
        now.nanosecond()
    )
}

fn create_dest_root(dest_root: &Path) -> Result<(), BackupError> {
    let existed = fs::symlink_metadata(dest_root).is_ok();
    fs::create_dir_all(dest_root).map_err(|error| {
        BackupError::io(format!("cannot create {}", dest_root.display()), error)
    })?;
    if !existed {
        // Make the new destination itself durable; best effort, as the parent may be read-only.
        if let Some(parent) = std::path::absolute(dest_root)
            .ok()
            .and_then(|path| path.parent().map(Path::to_path_buf))
        {
            let _ = File::open(parent).and_then(|directory| directory.sync_all());
        }
    }
    Ok(())
}

/// `mktemp -d DEST_ROOT/.incomplete-STAMP-XXXXXX`: a new private directory with a random
/// suffix. The suffix also names the final directory, so concurrent runs never collide.
fn create_stage(dest_root: &Path, stamp: &str) -> Result<(Stage, String), BackupError> {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    for _ in 0..100 {
        let mut bytes = [0_u8; 6];
        getrandom::fill(&mut bytes).map_err(|error| {
            BackupError::io(
                "cannot draw a staging name",
                io::Error::other(error.to_string()),
            )
        })?;
        let suffix: String = bytes
            .iter()
            .map(|byte| char::from(ALPHABET[usize::from(*byte) % ALPHABET.len()]))
            .collect();
        let path = dest_root.join(format!("{STAGING_PREFIX}{stamp}-{suffix}"));
        match DirBuilder::new().mode(STAGE_MODE).create(&path) {
            Ok(()) => {
                // The mode passed to mkdir is masked by the umask; set it exactly.
                let stage = Stage { path, armed: true };
                fs::set_permissions(&stage.path, fs::Permissions::from_mode(STAGE_MODE)).map_err(
                    |error| {
                        BackupError::io(format!("cannot secure {}", stage.path.display()), error)
                    },
                )?;
                return Ok((stage, suffix));
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(BackupError::io(
                    format!(
                        "cannot create a staging directory in {}",
                        dest_root.display()
                    ),
                    error,
                ));
            }
        }
    }
    Err(BackupError::io(
        format!(
            "cannot create a staging directory in {}",
            dest_root.display()
        ),
        io::Error::from(io::ErrorKind::AlreadyExists),
    ))
}

/// `VACUUM INTO` the snapshot file. The source is opened read-write without create, as the
/// `sqlite3` shell did, so a WAL database is read through its `-wal` and `-shm` files.
fn snapshot_database(source: &Path, snapshot: &Path, cancel: &Cancel) -> Result<(), BackupError> {
    let database = |action: &str, source: rusqlite::Error| BackupError::Database {
        action: action.to_owned(),
        source,
    };
    // An absolute target can never be read as a `file:` URI.
    let target = std::path::absolute(snapshot)
        .map_err(|error| BackupError::io("cannot resolve the snapshot path", error))?;
    let target = target.to_str().ok_or_else(|| {
        BackupError::io(
            "cannot snapshot the database",
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("destination path is not valid UTF-8: {}", target.display()),
            ),
        )
    })?;
    let connection = Connection::open_with_flags(
        source,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|error| database("cannot open artifacts.db", error))?;
    connection
        .busy_timeout(SNAPSHOT_BUSY_TIMEOUT)
        .map_err(|error| database("cannot configure artifacts.db", error))?;
    cancel.attach(&connection)?;
    let result = connection.execute("VACUUM INTO ?1", [target]);
    cancel.detach();
    match result {
        Ok(_) => Ok(()),
        Err(_) if cancel.is_cancelled() => Err(BackupError::Interrupted),
        Err(error) => Err(database("database snapshot failed", error)),
    }
}

fn sync_directory(directory: &Path) -> Result<(), BackupError> {
    File::open(directory)
        .and_then(|handle| handle.sync_all())
        .map_err(|error| BackupError::io(format!("cannot sync {}", directory.display()), error))
}

/// Remove a file, symlink, or directory tree. A directory left without owner write or search
/// permission (copied from such a source) is made removable first.
fn remove_tree(path: &Path) -> io::Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if !metadata.is_dir() {
        return fs::remove_file(path);
    }
    match fs::remove_dir_all(path) {
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
            make_removable(path);
            fs::remove_dir_all(path)
        }
        result => result,
    }
}

fn make_removable(directory: &Path) {
    if let Ok(metadata) = fs::symlink_metadata(directory)
        && metadata.is_dir()
    {
        let mode = metadata.permissions().mode() | 0o700;
        let _ = fs::set_permissions(directory, fs::Permissions::from_mode(mode));
        if let Ok(entries) = fs::read_dir(directory) {
            for entry in entries.flatten() {
                make_removable(&entry.path());
            }
        }
    }
}

/// Allocated bytes below `root`, without following symlinks, as `du -s` counts them.
fn disk_usage(root: &Path) -> io::Result<u64> {
    let metadata = fs::symlink_metadata(root)?;
    let mut total = metadata.blocks().saturating_mul(512);
    if metadata.is_dir() {
        for entry in fs::read_dir(root)? {
            total = total.saturating_add(disk_usage(&entry?.path())?);
        }
    }
    Ok(total)
}

/// `du -h` style: one decimal below 10, otherwise whole units, always rounded up.
fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["K", "M", "G", "T", "P", "E"];
    if bytes < 1024 {
        return bytes.to_string();
    }
    #[allow(clippy::cast_precision_loss)]
    let mut value = bytes as f64 / 1024.0;
    let mut unit = 0;
    loop {
        if value < 10.0 {
            let tenths = (value * 10.0).ceil();
            if tenths < 100.0 {
                return format!("{:.1}{}", tenths / 10.0, UNITS[unit]);
            }
        }
        let whole = value.ceil();
        if whole < 1024.0 || unit + 1 == UNITS.len() {
            return format!("{whole:.0}{}", UNITS[unit]);
        }
        value /= 1024.0;
        unit += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keep_accepts_the_former_pattern_only() {
        assert_eq!(parse_keep("14").unwrap(), 14);
        assert_eq!(parse_keep("007").unwrap(), 7);
        assert_eq!(parse_keep("1").unwrap(), 1);
        assert_eq!(
            parse_keep("99999999999999999999999999").unwrap(),
            usize::MAX
        );
        for invalid in ["", "0", "00", "-1", "+1", "1.5", " 1", "invalid", "1e3"] {
            assert!(
                matches!(parse_keep(invalid), Err(BackupError::InvalidKeep)),
                "{invalid:?}"
            );
        }
    }

    #[test]
    fn stamps_sort_in_time_order() {
        let early = OffsetDateTime::from_unix_timestamp_nanos(1_761_000_000_000_000_007).unwrap();
        let late = OffsetDateTime::from_unix_timestamp_nanos(1_761_000_001_000_000_000).unwrap();
        assert_eq!(stamp(early), "20251020-224000-000000007");
        assert!(stamp(early) < stamp(late));
    }

    #[test]
    fn sizes_render_like_du() {
        assert_eq!(human_size(0), "0");
        assert_eq!(human_size(512), "512");
        assert_eq!(human_size(4096), "4.0K");
        assert_eq!(human_size(4097), "4.1K");
        assert_eq!(human_size(10 * 1024), "10K");
        assert_eq!(human_size(10_240 * 1024 - 1), "10M");
        assert_eq!(human_size(1024 * 1024 * 1024), "1.0G");
    }
}
