//! Copying a data directory into a staging directory, and syncing the staged tree.
//!
//! Copies keep what `cp -a` kept for the entry types artifact storage uses: regular files,
//! directories, and symlinks (as symlinks), with permission bits, access and modification times,
//! and, where the process may set it, ownership. Ownership matters when a root-run backup is
//! restored for a server that runs as another user.

use std::{
    collections::BTreeMap,
    fs::{self, DirBuilder, File, FileTimes, Metadata, OpenOptions},
    io::{self, Read as _, Write as _},
    os::unix::fs::{
        DirBuilderExt as _, MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _,
    },
    path::Path,
};

use sha2::{Digest as _, Sha256};

use super::{BackupError, BackupObserver, Cancel, PREVIEW_MANIFEST};
use crate::artifacts::paths::{ARTIFACT_DIR_NAME, PREVIEW_DIR_NAME};

/// Read and write buffer size.
const CHUNK: usize = 1024 * 1024;
/// Mode of a directory while its entries are copied into it. The source mode is applied after.
const BUILD_MODE: u32 = 0o700;

const PREVIEWS_INCOMPLETE: &str = "optional preview copy incomplete; checking files included";

fn is_not_found(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::NotFound
}

/// Copy `DATA_DIR/artifacts` to `STAGE/artifacts` when it exists.
///
/// Every failure is fatal except an entry that vanished between listing and copying: the
/// database snapshot, not the file system, decides which bodies are required, and verification
/// fails if a required one is absent.
pub(super) fn copy_artifacts(
    data_dir: &Path,
    stage: &Path,
    cancel: &Cancel,
    observer: &mut dyn BackupObserver,
) -> Result<(), BackupError> {
    let source = data_dir.join(ARTIFACT_DIR_NAME);
    // `[ -e artifacts ]`: a dangling symlink counts as absent.
    if fs::metadata(&source).is_err() {
        return Ok(());
    }
    copy_entry(&source, &stage.join(ARTIFACT_DIR_NAME), cancel, observer)
}

fn copy_entry(
    source: &Path,
    target: &Path,
    cancel: &Cancel,
    observer: &mut dyn BackupObserver,
) -> Result<(), BackupError> {
    cancel.check()?;
    let metadata = match fs::symlink_metadata(source) {
        Ok(metadata) => metadata,
        Err(error) if is_not_found(&error) => return Ok(()),
        Err(error) => return Err(read_error(source, error)),
    };
    let kind = metadata.file_type();
    if kind.is_symlink() {
        return match copy_symlink(source, target) {
            Err(error) if is_not_found(&error) => Ok(()),
            result => result.map_err(|error| write_error(target, error)),
        };
    }
    if kind.is_dir() {
        create_build_dir(target)?;
        let entries = match fs::read_dir(source) {
            Ok(entries) => entries,
            Err(error) if is_not_found(&error) => return Ok(()),
            Err(error) => return Err(read_error(source, error)),
        };
        for entry in entries {
            let entry = entry.map_err(|error| read_error(source, error))?;
            copy_entry(
                &entry.path(),
                &target.join(entry.file_name()),
                cancel,
                observer,
            )?;
        }
        return apply_metadata(target, &metadata).map_err(|error| write_error(target, error));
    }
    if kind.is_file() {
        return match fs::copy(source, target) {
            Ok(_) => apply_metadata(target, &metadata).map_err(|error| write_error(target, error)),
            Err(error) if is_not_found(&error) && fs::symlink_metadata(source).is_err() => {
                let _ = fs::remove_file(target);
                Ok(())
            }
            Err(error) => Err(BackupError::io(
                format!("cannot copy {} to {}", source.display(), target.display()),
                error,
            )),
        };
    }
    observer.warning(&format!(
        "skipped unsupported entry (not a file, directory, or symlink): {}",
        source.display()
    ));
    Ok(())
}

/// Copy `DATA_DIR/previews` to `STAGE/previews`, hashing each regular file from the bytes read
/// for the copy, and write [`PREVIEW_MANIFEST`]. Verification later compares every included
/// preview with that manifest.
///
/// Previews are an optional cache. A preview that disappears, or that cannot be copied, is left
/// out with a warning. Symlinks are copied as symlinks and never become required content. A
/// `previews` path that is not a directory, or that contains a device, FIFO, or socket, fails
/// the run as before.
pub(super) fn copy_previews(
    data_dir: &Path,
    stage: &Path,
    cancel: &Cancel,
    observer: &mut dyn BackupObserver,
) -> Result<(), BackupError> {
    let source = data_dir.join(PREVIEW_DIR_NAME);
    let target = stage.join(PREVIEW_DIR_NAME);
    let mut copy = PreviewCopy {
        cancel,
        observer,
        files: BTreeMap::new(),
        incomplete: false,
    };
    match fs::symlink_metadata(&source) {
        Err(error) if is_not_found(&error) => {}
        Err(error) => return Err(read_error(&source, error)),
        Ok(metadata) if metadata.file_type().is_symlink() => {
            // An opaque link to an existing cache is kept as a link, as `cp -a` kept it.
            if fs::metadata(&source).is_ok()
                && let Err(error) = copy_symlink(&source, &target)
                && !is_not_found(&error)
            {
                copy.incomplete = true;
            }
        }
        Ok(metadata) if metadata.is_dir() => copy.directory(&source, &target, "", &metadata)?,
        Ok(_) => {
            return Err(BackupError::coherence(
                "preview cache must be a directory when included",
            ));
        }
    }
    if copy.incomplete {
        copy.observer.warning(PREVIEWS_INCOMPLETE);
    }
    let manifest = serde_json::json!({ "files": copy.files, "version": 1 });
    let manifest_path = stage.join(PREVIEW_MANIFEST);
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&manifest_path)
        .and_then(|mut file| writeln!(file, "{manifest}"))
        .map_err(|error| write_error(&manifest_path, error))
}

struct PreviewCopy<'a> {
    cancel: &'a Cancel,
    observer: &'a mut dyn BackupObserver,
    files: BTreeMap<String, String>,
    incomplete: bool,
}

impl PreviewCopy<'_> {
    fn directory(
        &mut self,
        source: &Path,
        target: &Path,
        relative: &str,
        metadata: &Metadata,
    ) -> Result<(), BackupError> {
        if DirBuilder::new().mode(BUILD_MODE).create(target).is_err() {
            self.incomplete = true;
            return Ok(());
        }
        let entries = match fs::read_dir(source) {
            Ok(entries) => entries,
            Err(error) => {
                self.incomplete |= !is_not_found(&error);
                return Ok(());
            }
        };
        for entry in entries {
            self.cancel.check()?;
            let Ok(entry) = entry else {
                self.incomplete = true;
                continue;
            };
            let name = entry.file_name();
            let child_relative = if relative.is_empty() {
                name.to_string_lossy().into_owned()
            } else {
                format!("{relative}/{}", name.to_string_lossy())
            };
            let child_source = entry.path();
            let child_target = target.join(&name);
            let child_metadata = match fs::symlink_metadata(&child_source) {
                Ok(child_metadata) => child_metadata,
                Err(error) => {
                    self.incomplete |= !is_not_found(&error);
                    continue;
                }
            };
            let kind = child_metadata.file_type();
            if kind.is_symlink() {
                if let Err(error) = copy_symlink(&child_source, &child_target) {
                    self.incomplete |= !is_not_found(&error);
                }
            } else if kind.is_dir() {
                self.directory(
                    &child_source,
                    &child_target,
                    &child_relative,
                    &child_metadata,
                )?;
            } else if kind.is_file() {
                match copy_file_hashing(&child_source, &child_target, &child_metadata, self.cancel)
                {
                    Ok(digest) => {
                        self.files.insert(child_relative, digest);
                    }
                    Err(error) => {
                        let _ = fs::remove_file(&child_target);
                        self.cancel.check()?;
                        self.incomplete |= !is_not_found(&error);
                    }
                }
            } else {
                return Err(BackupError::coherence(
                    "preview cache contains an unsupported entry",
                ));
            }
        }
        if apply_metadata(target, metadata).is_err() {
            self.incomplete = true;
        }
        Ok(())
    }
}

/// Copy one regular file and return the SHA-256 of the bytes read, which are the bytes written.
fn copy_file_hashing(
    source: &Path,
    target: &Path,
    metadata: &Metadata,
    cancel: &Cancel,
) -> io::Result<String> {
    let mut input = File::open(source)?;
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(target)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; CHUNK];
    loop {
        if cancel.is_cancelled() {
            return Err(io::Error::from(io::ErrorKind::Interrupted));
        }
        let read = match input.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        hasher.update(&buffer[..read]);
        output.write_all(&buffer[..read])?;
    }
    drop(output);
    apply_metadata(target, metadata)?;
    Ok(hex::encode(hasher.finalize()))
}

fn copy_symlink(source: &Path, target: &Path) -> io::Result<()> {
    let destination = fs::read_link(source)?;
    std::os::unix::fs::symlink(destination, target)?;
    let metadata = fs::symlink_metadata(source)?;
    chown_best_effort(target, &metadata)
}

fn create_build_dir(target: &Path) -> Result<(), BackupError> {
    DirBuilder::new()
        .mode(BUILD_MODE)
        .create(target)
        .map_err(|error| write_error(target, error))
}

/// Times first (they need the owner's write access to a file we created), then the mode, then
/// the owner.
fn apply_metadata(target: &Path, metadata: &Metadata) -> io::Result<()> {
    let times = FileTimes::new()
        .set_accessed(metadata.accessed()?)
        .set_modified(metadata.modified()?);
    File::open(target)?.set_times(times)?;
    fs::set_permissions(
        target,
        fs::Permissions::from_mode(metadata.permissions().mode() & 0o7777),
    )?;
    chown_best_effort(target, metadata)
}

/// Keep the source owner where the process may; an unprivileged run keeps its own.
fn chown_best_effort(target: &Path, metadata: &Metadata) -> io::Result<()> {
    let current = fs::symlink_metadata(target)?;
    if current.uid() == metadata.uid() && current.gid() == metadata.gid() {
        return Ok(());
    }
    match std::os::unix::fs::lchown(target, Some(metadata.uid()), Some(metadata.gid())) {
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => Ok(()),
        result => result,
    }
}

/// fsync every regular file and directory below `root`, children before parents, then `root`.
/// Symlinks and special files are not opened.
pub(super) fn sync_tree(root: &Path, cancel: &Cancel) -> Result<(), BackupError> {
    for entry in fs::read_dir(root).map_err(|error| read_error(root, error))? {
        cancel.check()?;
        let entry = entry.map_err(|error| read_error(root, error))?;
        let path = entry.path();
        let kind = entry
            .file_type()
            .map_err(|error| read_error(&path, error))?;
        if kind.is_dir() {
            sync_tree(&path, cancel)?;
        } else if kind.is_file() {
            File::open(&path)
                .and_then(|file| file.sync_all())
                .map_err(|error| {
                    BackupError::io(format!("cannot sync {}", path.display()), error)
                })?;
        }
    }
    File::open(root)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| BackupError::io(format!("cannot sync {}", root.display()), error))
}

fn read_error(path: &Path, error: io::Error) -> BackupError {
    BackupError::io(format!("cannot read {}", path.display()), error)
}

fn write_error(path: &Path, error: io::Error) -> BackupError {
    BackupError::io(format!("cannot write {}", path.display()), error)
}
