use std::path::{Path, PathBuf};

use rusqlite::Connection;

use crate::artifacts::lifecycle::{InjectedFault, io_failure, sql_error};
use crate::artifacts::paths::{self, SafeArtifactId, TransientKind};
use crate::error::AppError;

/// One row of the current-body reconciliation query — [lib/store.js:120]
#[derive(Debug, Clone)]
pub(super) struct BodyRow {
    pub(super) id: String,
    pub(super) safe_id: Option<SafeArtifactId>,
    pub(super) is_bundle: bool,
    pub(super) body_sha256: String,
    pub(super) revision: u64,
    pub(super) outgoing_is_bundle: Option<bool>,
}

impl BodyRow {
    /// `row.is_bundle ? bundleDir(row.id) : filePath(row.id)` — [lib/store.js:700]
    ///
    /// `None` when the stored id cannot be a path segment. Every generated id satisfies
    /// `/^[0-9a-z]{6,24}$/` ([lib/store.js:30]), so this only guards a hand-edited database.
    pub(super) fn body_path(&self, artifact_dir: &Path) -> Option<PathBuf> {
        self.safe_id
            .as_ref()
            .map(|id| paths::body_path(artifact_dir, id, self.is_bundle))
    }

    /// ``rows.map((row) => row.is_bundle ? row.id : `${row.id}.html`)`` — [lib/store.js:678]
    pub(super) fn expected_name(&self) -> String {
        if self.is_bundle {
            self.id.clone()
        } else {
            format!("{}.{}", self.id, paths::SINGLE_BODY_EXTENSION)
        }
    }
}

pub(super) fn body_row_from(
    row: &rusqlite::Row<'_>,
    with_digest: bool,
) -> rusqlite::Result<BodyRow> {
    let id: String = row.get(0)?;
    let safe_id = SafeArtifactId::parse(&id);
    Ok(BodyRow {
        id,
        safe_id,
        is_bundle: row.get::<_, i64>(1)? != 0,
        body_sha256: if with_digest {
            row.get::<_, Option<String>>(2)?.unwrap_or_default()
        } else {
            String::new()
        },
        revision: if with_digest {
            row.get::<_, i64>(3)?.unsigned_abs()
        } else {
            0
        },
        outgoing_is_bundle: if with_digest {
            row.get::<_, Option<i64>>(4)?.map(|value| value != 0)
        } else {
            None
        },
    })
}

pub(super) fn load_body_rows(conn: &Connection) -> Result<Vec<BodyRow>, AppError> {
    let mut statement = conn
        .prepare(
            "SELECT a.id, a.is_bundle, a.body_sha256, a.revision, \
                    (SELECT r.is_bundle FROM artifact_revisions r \
                     WHERE r.artifact_id = a.id AND r.revision = a.revision - 1) \
             FROM artifacts a",
        )
        .map_err(|error| sql_error("prepare storage audit query", &error))?;
    statement
        .query_map([], |row| body_row_from(row, true))
        .map_err(|error| sql_error("query storage audit rows", &error))?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|error| sql_error("read storage audit rows", &error))
}

/// Directory entries in Node's processing order: `.staging-` first, then `.trash-`, then
/// everything else. [lib/store.js:688-691]
///
/// Node keeps `readdirSync` order inside a rank because `Array.prototype.sort` is stable; this
/// sorts by name inside a rank instead, which is deterministic across runtimes and filesystems.
/// The two can only differ when a single artifact has two transient paths of the same kind, an
/// order the reference does not define either.
pub(super) fn ranked_entries(artifact_dir: &Path) -> Result<Vec<String>, AppError> {
    let mut names: Vec<String> = Vec::new();
    let entries = match std::fs::read_dir(artifact_dir) {
        Ok(entries) => entries,
        // Node would throw; at startup a missing directory just means an empty store, and both
        // `lib/db.js` and `Database::open_at` create it.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(names),
        Err(error) => return Err(io_failure("list artifact directory", artifact_dir, &error)),
    };
    for entry in entries {
        let entry = entry
            .map_err(|error| io_failure("read artifact directory entry", artifact_dir, &error))?;
        names.push(entry.file_name().to_string_lossy().into_owned());
    }
    names.sort_by(|left, right| {
        transient_rank(left)
            .cmp(&transient_rank(right))
            .then_with(|| left.cmp(right))
    });
    Ok(names)
}

/// `const rank = (n) => (n.includes(".staging-") ? 0 : n.includes(".trash-") ? 1 : 2);`
/// — [lib/store.js:689]
pub(super) fn transient_rank(name: &str) -> u8 {
    if name.contains(&staging_marker()) {
        0
    } else if name.contains(&trash_marker()) {
        1
    } else {
        2
    }
}

/// `".staging-"` — [lib/store.js:689], [lib/store.js:705]
pub(super) fn staging_marker() -> String {
    format!(".{}-", TransientKind::Staging.as_str())
}

/// `".trash-"` — [lib/store.js:689]
pub(super) fn trash_marker() -> String {
    format!(".{}-", TransientKind::Trash.as_str())
}
pub(super) fn into_error(fault: InjectedFault) -> AppError {
    match fault {
        InjectedFault::Error(error) | InjectedFault::Crash(error) => error,
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranks_staging_before_trash_before_everything_else() {
        assert_eq!(transient_rank(".abc123.staging-xyz"), 0);
        assert_eq!(transient_rank(".abc123.trash-xyz"), 1);
        assert_eq!(transient_rank("abc123.html"), 2);
        assert_eq!(transient_rank(paths::HISTORY_DIR_NAME), 2);
    }

    #[test]
    fn expected_names_follow_the_frozen_layout() {
        let single = BodyRow {
            id: "abc123def456".to_owned(),
            safe_id: SafeArtifactId::parse("abc123def456"),
            is_bundle: false,
            body_sha256: String::new(),
            revision: 1,
            outgoing_is_bundle: None,
        };
        let bundle = BodyRow {
            is_bundle: true,
            ..single.clone()
        };
        assert_eq!(single.expected_name(), "abc123def456.html");
        assert_eq!(bundle.expected_name(), "abc123def456");
        assert_eq!(
            single.body_path(Path::new("/data/artifacts")),
            Some(PathBuf::from("/data/artifacts/abc123def456.html"))
        );
        assert_eq!(
            bundle.body_path(Path::new("/data/artifacts")),
            Some(PathBuf::from("/data/artifacts/abc123def456"))
        );
    }
}
