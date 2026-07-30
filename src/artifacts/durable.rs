//! Filesystem durability barriers for artifact bodies.
//!
//! SQLite WAL `synchronous=FULL` only makes metadata durable. These helpers establish matching
//! body-side barriers and return errors rather than silently weakening the advertised guarantee.

use std::fs::{self, File};
use std::path::Path;

use crate::error::AppError;

use super::lifecycle::io_failure;

/// Flush a body file's contents and metadata.
pub fn sync_file(path: &Path) -> Result<(), AppError> {
    sync_file_with_barrier(path, || Ok(()))
}

/// Flush a body file after its descriptor is open but before `sync_all` executes.
///
/// The hook is intentionally inside the primitive rather than at lifecycle call sites: tests can
/// model a real file-sync syscall failure without changing whether the path was opened first.
pub fn sync_file_with_barrier(
    path: &Path,
    before_sync: impl FnOnce() -> Result<(), AppError>,
) -> Result<(), AppError> {
    let file = File::open(path)
        .map_err(|error| io_failure("open artifact file for sync", path, &error))?;
    before_sync()?;
    file.sync_all()
        .map_err(|error| io_failure("sync artifact file", path, &error))
}

/// Flush a directory entry change. Platforms that cannot sync directories fail closed.
pub fn sync_dir(path: &Path) -> Result<(), AppError> {
    sync_dir_with_barrier(path, || Ok(()))
}

/// Flush a directory after it is open but before `sync_all` executes.
///
/// This matches [`sync_file_with_barrier`]'s injection seam and is used to prove that directory
/// fsync failures leave lifecycle intents recoverable instead of being acknowledged.
pub fn sync_dir_with_barrier(
    path: &Path,
    before_sync: impl FnOnce() -> Result<(), AppError>,
) -> Result<(), AppError> {
    let directory = File::open(path).map_err(|error| {
        io_failure(
            "open artifact directory for sync (local POSIX filesystem required)",
            path,
            &error,
        )
    })?;
    before_sync()?;
    directory.sync_all().map_err(|error| {
        io_failure(
            "sync artifact directory (local POSIX filesystem required)",
            path,
            &error,
        )
    })
}

/// Create a directory chain without acknowledging a newly-created child until its parent entry
/// is durable. `create_dir_all` alone leaves `.history` or `.history/<artifact>` vulnerable to
/// power loss: syncing only the child cannot persist the child's name in its parent.
pub fn create_dir_all(root: &Path, path: &Path) -> Result<(), AppError> {
    create_dir_all_with_barrier(root, path, || Ok(()))
}

/// [`create_dir_all`] with a first-barrier seam for lifecycle crash proofs.
pub fn create_dir_all_with_barrier(
    root: &Path,
    path: &Path,
    before_first_sync: impl FnOnce() -> Result<(), AppError>,
) -> Result<(), AppError> {
    let relative = path.strip_prefix(root).map_err(|_| AppError::Internal)?;
    if !root.is_dir() {
        return Err(AppError::Internal);
    }
    let mut directory = root.to_path_buf();
    let mut before_first_sync = Some(before_first_sync);
    for component in relative.components() {
        directory.push(component);
        let parent = directory.parent().ok_or(AppError::Internal)?;
        if !directory.exists() {
            match fs::create_dir(&directory) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    if !directory.is_dir() {
                        return Err(io_failure("create artifact directory", &directory, &error));
                    }
                }
                Err(error) => {
                    return Err(io_failure("create artifact directory", &directory, &error));
                }
            }
        }
        // Sync every entry from the known artifact root, including entries that survived an
        // earlier failed attempt. Otherwise retrying after `.history` exists in-process could
        // sync only `.history/<id>` and never persist `.history` in the artifact root.
        sync_dir_with_barrier(parent, || {
            before_first_sync.take().map_or(Ok(()), |hook| hook())
        })?;
        sync_dir_with_barrier(&directory, || {
            before_first_sync.take().map_or(Ok(()), |hook| hook())
        })?;
    }
    Ok(())
}

/// Flush every leaf and directory in a bundle tree bottom-up.
pub fn sync_tree(path: &Path) -> Result<(), AppError> {
    sync_tree_with_barrier(path, || Ok(()))
}

/// Flush every leaf and directory in a bundle tree, injecting one real `sync_all` failure point.
pub fn sync_tree_with_barrier(
    path: &Path,
    before_first_sync: impl FnOnce() -> Result<(), AppError>,
) -> Result<(), AppError> {
    let mut before_first_sync = Some(before_first_sync);
    sync_tree_inner(path, &mut before_first_sync)
}

fn sync_tree_inner<F>(path: &Path, before_first_sync: &mut Option<F>) -> Result<(), AppError>
where
    F: FnOnce() -> Result<(), AppError>,
{
    let metadata = fs::metadata(path)
        .map_err(|error| io_failure("stat artifact body for sync", path, &error))?;
    if !metadata.is_dir() {
        return sync_file_with_barrier(path, || {
            before_first_sync.take().map_or(Ok(()), |hook| hook())
        });
    }
    for entry in fs::read_dir(path)
        .map_err(|error| io_failure("read artifact tree for sync", path, &error))?
    {
        let entry = entry.map_err(|error| io_failure("read artifact tree entry", path, &error))?;
        sync_tree_inner(&entry.path(), before_first_sync)?;
    }
    sync_dir_with_barrier(path, || {
        before_first_sync.take().map_or(Ok(()), |hook| hook())
    })
}

/// Same-volume rename followed by durable parent directory updates.
pub fn rename(from: &Path, to: &Path) -> Result<(), AppError> {
    rename_after_move(from, to, || Ok(()))
}

/// Same as [`rename`], with an injectable barrier immediately after the kernel rename and before
/// either directory fsync.  Lifecycle tests use this to model the critical physical-partial
/// state: destination exists, source is gone, but acknowledgement must not proceed.
pub fn rename_after_move(
    from: &Path,
    to: &Path,
    after_move: impl FnOnce() -> Result<(), AppError>,
) -> Result<(), AppError> {
    fs::rename(from, to).map_err(|error| {
        io_failure(
            "rename artifact body (same filesystem required)",
            to,
            &error,
        )
    })?;
    after_move()?;
    let from_parent = from.parent().ok_or(AppError::Internal)?;
    let to_parent = to.parent().ok_or(AppError::Internal)?;
    sync_dir(to_parent)?;
    if from_parent != to_parent {
        sync_dir(from_parent)?;
    }
    Ok(())
}

/// Remove a path and durably record its parent-directory change.
pub fn remove(path: &Path) -> Result<(), AppError> {
    remove_with_barrier(path, || Ok(()))
}

/// Remove a path with an injectable boundary after unlink and before the parent fsync.
pub fn remove_with_barrier(
    path: &Path,
    after_remove: impl FnOnce() -> Result<(), AppError>,
) -> Result<(), AppError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(value) => value,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(io_failure("stat artifact removal", path, &error)),
    };
    if metadata.is_dir() {
        fs::remove_dir_all(path)
            .map_err(|error| io_failure("remove artifact tree", path, &error))?;
    } else {
        fs::remove_file(path).map_err(|error| io_failure("remove artifact file", path, &error))?;
    }
    after_remove()?;
    if let Some(parent) = path.parent() {
        sync_dir(parent)?;
    }
    Ok(())
}

/// Confirm a path is absent and that an existing parent records that absence durably.
///
/// A previous [`remove`] may have unlinked the path and then failed its directory fsync. On the
/// recovery retry the path is already absent, but the parent still needs a barrier before the
/// durability intent may be released.
pub fn ensure_removed(path: &Path) -> Result<(), AppError> {
    match fs::symlink_metadata(path) {
        Ok(_) => remove(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if let Some(parent) = path.parent()
                && parent.is_dir()
            {
                sync_dir(parent)?;
            }
            Ok(())
        }
        Err(error) => Err(io_failure("stat artifact removal", path, &error)),
    }
}
