//! Filesystem primitives shared by the lifecycle and reconciliation.

use super::*;

// ---------------------------------------------------------------------------
// Filesystem primitives
// ---------------------------------------------------------------------------

/// `files.existsSync(target)` — `stat`, so a broken symlink counts as absent.
/// [lib/store.js:186]
#[must_use]
pub(crate) fn path_exists(target: &Path) -> bool {
    std::fs::metadata(target).is_ok()
}

/// `safeRemove(files, target)` = `rmSync(target, { recursive: true, force: true })` inside a
/// swallowing `try`. [lib/store.js:71-75]
pub(crate) fn safe_remove(target: &Path) {
    // Cleanup remains best-effort, but a successful cleanup gets a durable parent barrier.
    let _ = durable::remove(target);
}

/// `files.renameSync(from, to)` — a hard failure, never swallowed.
pub(crate) fn rename(from: &Path, to: &Path) -> Result<(), AppError> {
    durable::rename(from, to)
}

/// `files.cpSync(src, dest, { recursive: true })` — [lib/store.js:559]
pub(crate) fn copy_recursive(source: &Path, destination: &Path) -> std::io::Result<()> {
    if std::fs::metadata(source)?.is_dir() {
        std::fs::create_dir_all(destination)?;
        for entry in std::fs::read_dir(source)? {
            let entry = entry?;
            copy_recursive(&entry.path(), &destination.join(entry.file_name()))?;
        }
        return Ok(());
    }
    if let Some(parent) = destination.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::copy(source, destination)?;
    Ok(())
}

pub(crate) fn io_failure(operation: &'static str, path: &Path, error: &std::io::Error) -> AppError {
    tracing::error!(
        operation,
        path = %path.display(),
        error = %error,
        "artifact filesystem operation failed"
    );
    AppError::Internal
}
