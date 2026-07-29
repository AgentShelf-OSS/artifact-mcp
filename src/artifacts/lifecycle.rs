//! Owned by U08 (sol) — artifact publish, update, restore, and delete lifecycle.
//!
//! This is the cross-resource unit: every mutation touches SQLite *and* the filesystem, and no
//! transaction can span both. ADR-0002 (`docs/adr/0002-sqlite-filesystem-artifact-lifecycle.md`)
//! fixes the orderings that make each interruption recoverable, and the Node reference
//! (`lib/store.js`) is the normative implementation of them. **The ordering is deliberate, not
//! incidental**; the table below is the contract this module preserves.
//!
//! | Operation | Ordered steps | Node oracle |
//! |---|---|---|
//! | publish | write staging body → insert row → rename staging into place | [lib/store.js:216-221], [lib/store.js:262-274] |
//! | publish compensation | delete row (if inserted) → remove staging → remove final | [lib/store.js:222-227], [lib/store.js:275-280] |
//! | update | validate+digest → *no-op returns without a revision* → stage body → **commit metadata** → snapshot outgoing body → swap staged body → prune history | [lib/store.js:343-438] |
//! | update compensation | restore snapshot body → remove staging → delete revision row + revert metadata | [lib/store.js:429-437] |
//! | restore | read revision row → read history body → replay through `update` as a NEW revision | [lib/store.js:605-624] |
//! | delete | move body to trash → delete row → remove trash → remove history | [lib/store.js:626-644] |
//! | delete compensation | move the body back out of trash | [lib/store.js:635], [lib/store.js:642] |
//! | move/re-tenant | one transaction with `defer_foreign_keys` → artifact, feedback, revisions, views, then revoke shares | [lib/store.js:469-477] |
//!
//! The single most important choice is **commit-then-swap** in `update` ([lib/store.js:386-393]):
//! a crash between the metadata commit and the body swap leaves committed metadata, the *old*
//! body still installed, and the *new* body in staging — a state
//! [`crate::artifacts::reconciliation::audit_storage`] repairs by digest. The reverse ordering
//! (swap inside the transaction) would roll metadata back while keeping the new file, and startup
//! would then delete the only copy of the old body.
//!
//! # Blocking discipline
//!
//! Every method is one synchronous closure handed to [`crate::persistence::db::interact`], which
//! runs it on `spawn_blocking` with an exclusive pooled connection that is dropped before the
//! future resolves. SQL and the filesystem work it coordinates happen inside that single closure,
//! so no connection or transaction is ever held across an `.await`.
//!
//! # Injected failpoints
//!
//! Compensation ordering is only trustworthy if it is exercised. [`FaultInjector`] makes every
//! write, transaction, rename, snapshot, delete, and compensation step individually failable, in
//! two flavours: [`InjectedFault::Error`] (an in-process failure, which compensates) and
//! [`InjectedFault::Crash`] (the process died here, so nothing compensates and startup
//! reconciliation has to repair it). Production composes [`NoFaults`].
//!
//! # Contract-delta requests raised by U08
//!
//! 1. **`ArtifactUpdate` cannot express Node's "entry-only bundle update".** Node distinguishes
//!    `files === undefined && entry !== undefined` ([lib/store.js:354-360]) from `files === {}`
//!    (which is the error `files is empty`). The frozen model has a single
//!    `content: Option<ArtifactContent>`, so this module reads
//!    `Bundle { files: [], entry: Some(_) }` as the entry-only update and
//!    `Bundle { files: [], entry: None }` as `files is empty`. The one divergent input is a
//!    caller that supplies *both* an empty file map and an entry: Node reports `files is empty`,
//!    this reports success on the entry change. U13 must map MCP arguments accordingly.
//! 2. **A bundle path that sanitizes to a trailing separator is an I/O failure, not a validated
//!    rejection.** `sanitizeRel("a/b/")` returns `"a/b/"` ([lib/store.js:50-55]) and Node then
//!    fails inside `writeFileSync` with `EISDIR`; Rust's `Path::join` would silently drop the
//!    separator and create a *file*. [`StoreContext::stage_bundle_files`] therefore refuses such
//!    a name so both runtimes fail the publish. Fixing it properly means rejecting the name in
//!    `sanitizeRel` on both sides.
//! 3. **Lifecycle failures carry Node's `reason` token as their message** (`not_found`,
//!    `conflict`, `revision_not_found`, `type_mismatch`, `body_missing`). The HTTP restore route
//!    returns that token verbatim ([lib/app.js:718-719]) while the MCP tool renders a friendly
//!    sentence ([lib/mcp.js:517-523]), so the token is the only shape both consumers can derive
//!    theirs from. The `AppError` variant already encodes the status Node uses.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use rusqlite::{Connection, OptionalExtension, params};

use crate::config::{AppConfig, IdSource, StorageLimits};
use crate::error::AppError;
use crate::model::{
    ArtifactContent, ArtifactFile, ArtifactId, ArtifactMeta, ArtifactRevision, ArtifactUpdate,
    ClientId, DigestBackfillReport, OrgArtifacts, OrgId, PublishArtifact, PublishedArtifact,
    PublisherIdentity, RestoreArtifactResult, RevisionHistory, StorageAuditReport, Timestamp,
    UpdateArtifactResult,
};
use crate::persistence::db::{self, DbPool};
use crate::ports::{ArtifactService, BoxFuture};
use crate::security::access::AuthorizedArtifact;

use super::digest::bundle_manifest_digest;
use super::paths::{self, SafeArtifactId, TransientKind};
use super::read;
use super::reconciliation;
use super::validation::{
    self, ValidatedBundle, is_reserved_artifact_id, js_trim, sanitize_relative_path,
};

// ---------------------------------------------------------------------------
// Failpoints
// ---------------------------------------------------------------------------

/// Every interruptible boundary in the lifecycle, named for the step it precedes.
///
/// A fault raised at a point means "the step named by this variant never happened", which is
/// exactly the window a crash can land in. Compensation variants fire *during* the rollback of an
/// earlier fault, so a test can prove the double-failure path too.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FaultPoint {
    /// Before the staging body is written. [lib/store.js:217], [lib/store.js:263-267]
    PublishStageWrite,
    /// After staging, before `INSERT INTO artifacts`. [lib/store.js:218]
    PublishInsert,
    /// After the insert, before `rename(staging → final)`. [lib/store.js:220]
    PublishRename,
    /// After the rename, before the metadata read-back.
    PublishComplete,
    /// Compensation: before the inserted artifact row is deleted. [lib/store.js:223],
    /// [lib/store.js:276]
    PublishDeleteRow,
    /// Before the replacement body is staged. [lib/store.js:374-384]
    UpdateStageWrite,
    /// After staging, before the metadata transaction opens. [lib/store.js:406]
    UpdateCommit,
    /// Inside the metadata transaction, after the guarded UPDATE and the revision row, before
    /// the commit. [lib/store.js:412-414]
    UpdateCommitTransaction,
    /// After the metadata commit, before the outgoing body is snapshotted. [lib/store.js:424]
    UpdateSnapshot,
    /// After the snapshot, before the staged body is swapped in. [lib/store.js:426]
    UpdateSwap,
    /// After the swap, before history pruning. [lib/store.js:438]
    UpdatePrune,
    /// Compensation: before the snapshotted body is moved back. [lib/store.js:430]
    UpdateRestoreSnapshot,
    /// Compensation: before the revision row is dropped and metadata reverted.
    /// [lib/store.js:432-435]
    UpdateRevertMetadata,
    /// Before the live body is renamed into trash. [lib/store.js:503-509]
    DeleteTrashRename,
    /// After the body reached trash, before `DELETE FROM artifacts`. [lib/store.js:632]
    DeleteRow,
    /// After the row is gone, before the trashed body is removed. [lib/store.js:637]
    DeleteTrashRemove,
    /// After trash removal, before the history directory is removed. [lib/store.js:638]
    DeleteHistoryRemove,
    /// Compensation: before the trashed body is moved back. [lib/store.js:635], [lib/store.js:642]
    DeleteRestoreBody,
    /// Before the re-tenant transaction runs. [lib/store.js:469-477]
    MoveTransaction,
    /// Before a `set_category` / `set_hidden` write.
    MetadataWrite,
    /// Reconciliation: before an interrupted body is renamed to its final path.
    /// [lib/store.js:702], [lib/store.js:711]
    ReconcileRecover,
    /// Reconciliation: before an unreferenced transient path is removed. [lib/store.js:714]
    ReconcileDiscard,
    /// Reconciliation: before an orphan history directory is removed. [lib/store.js:728]
    ReconcileOrphanHistory,
    /// Before a backfilled digest is written. [lib/store.js:766]
    BackfillWrite,
}

/// The verdict an armed failpoint produces.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InjectedFault {
    /// An in-process failure: the operation compensates exactly as Node's `catch` block does.
    Error(AppError),
    /// The process died at this point: no compensation runs, and the on-disk/database state is
    /// left exactly as of the boundary for startup reconciliation to repair.
    Crash(AppError),
}

impl InjectedFault {
    fn into_error(self) -> AppError {
        match self {
            Self::Error(error) | Self::Crash(error) => error,
        }
    }
}

/// Test seam that turns any lifecycle boundary into a failure.
pub trait FaultInjector: Send + Sync + fmt::Debug {
    /// # Errors
    /// Returns the fault armed for `point`, if any.
    fn check(&self, point: FaultPoint) -> Result<(), InjectedFault>;
}

/// Production injector: never fails.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoFaults;

impl FaultInjector for NoFaults {
    fn check(&self, _point: FaultPoint) -> Result<(), InjectedFault> {
        Ok(())
    }
}

#[derive(Debug)]
struct ArmedFault {
    point: FaultPoint,
    crash: bool,
    remaining: usize,
}

/// A deterministic injector: arm a point, it fires the configured number of times, and every
/// visited point is recorded so a test can prove the step was actually reached.
#[derive(Debug, Default)]
pub struct ScriptedFaults {
    armed: Mutex<Vec<ArmedFault>>,
    visited: Mutex<Vec<FaultPoint>>,
}

impl ScriptedFaults {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Fail once at `point` with a recoverable in-process error.
    #[must_use]
    pub fn fail_once(self, point: FaultPoint) -> Self {
        self.arm(point, false, 1)
    }

    /// Simulate a process death once at `point`: no compensation runs.
    #[must_use]
    pub fn crash_once(self, point: FaultPoint) -> Self {
        self.arm(point, true, 1)
    }

    fn arm(self, point: FaultPoint, crash: bool, remaining: usize) -> Self {
        if let Ok(mut armed) = self.armed.lock() {
            armed.push(ArmedFault {
                point,
                crash,
                remaining,
            });
        }
        self
    }

    /// Every failpoint the store reached, in order.
    #[must_use]
    pub fn visited(&self) -> Vec<FaultPoint> {
        self.visited
            .lock()
            .map(|visited| visited.clone())
            .unwrap_or_default()
    }

    /// True when every armed fault has fired.
    #[must_use]
    pub fn all_fired(&self) -> bool {
        self.armed
            .lock()
            .is_ok_and(|armed| armed.iter().all(|entry| entry.remaining == 0))
    }
}

impl FaultInjector for ScriptedFaults {
    fn check(&self, point: FaultPoint) -> Result<(), InjectedFault> {
        if let Ok(mut visited) = self.visited.lock() {
            visited.push(point);
        }
        let Ok(mut armed) = self.armed.lock() else {
            return Ok(());
        };
        for entry in armed.iter_mut() {
            if entry.point == point && entry.remaining > 0 {
                entry.remaining -= 1;
                let error = AppError::Unavailable(format!("injected fault at {point:?}"));
                return Err(if entry.crash {
                    InjectedFault::Crash(error)
                } else {
                    InjectedFault::Error(error)
                });
            }
        }
        Ok(())
    }
}

/// An aborted operation, carrying whether Node's `catch` block should run.
#[derive(Debug)]
struct Interrupt {
    error: AppError,
    compensate: bool,
}

impl From<AppError> for Interrupt {
    fn from(error: AppError) -> Self {
        Self {
            error,
            compensate: true,
        }
    }
}

impl From<InjectedFault> for Interrupt {
    fn from(fault: InjectedFault) -> Self {
        let compensate = matches!(fault, InjectedFault::Error(_));
        Self {
            error: fault.into_error(),
            compensate,
        }
    }
}

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
    let Ok(metadata) = std::fs::symlink_metadata(target) else {
        return;
    };
    let _ = if metadata.is_dir() {
        std::fs::remove_dir_all(target)
    } else {
        std::fs::remove_file(target)
    };
}

/// `files.renameSync(from, to)` — a hard failure, never swallowed.
pub(crate) fn rename(from: &Path, to: &Path) -> Result<(), AppError> {
    std::fs::rename(from, to).map_err(|error| {
        tracing::error!(
            from = %from.display(),
            to = %to.display(),
            error = %error,
            "artifact rename failed"
        );
        AppError::Internal
    })
}

/// `files.cpSync(src, dest, { recursive: true })` — [lib/store.js:559]
fn copy_recursive(source: &Path, destination: &Path) -> std::io::Result<()> {
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

// ---------------------------------------------------------------------------
// JavaScript string semantics
// ---------------------------------------------------------------------------

/// ECMAScript `\s` / `WhiteSpace` + `LineTerminator`, matching U07's private predicate in
/// `validation.rs`. `String.prototype.trim` and the regex class cover the same set.
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

/// `String.prototype.slice(0, max)` — UTF-16 code units, not bytes and not `char`s.
///
/// A limit that would split a surrogate pair drops the whole astral character; JavaScript would
/// keep a lone surrogate, which Rust's `String` cannot represent and SQLite would not store
/// faithfully either.
#[must_use]
fn js_slice(value: &str, max_units: usize) -> &str {
    let mut units = 0;
    for (index, character) in value.char_indices() {
        let width = character.len_utf16();
        if units + width > max_units {
            return &value[..index];
        }
        units += width;
    }
    value
}

/// `String(title || "Untitled artifact").slice(0, 200)` — [lib/store.js:196]
const TITLE_MAX_UNITS: usize = 200;
/// `String(description || "").slice(0, 500)` — [lib/store.js:197]
const DESCRIPTION_MAX_UNITS: usize = 500;
/// `String(uploaderLabel || "").slice(0, 60)` — [lib/store.js:195]
const LABEL_MAX_UNITS: usize = 60;
/// `.slice(0, 60)` inside `normalizeCategory` — [lib/store.js:58]
const CATEGORY_MAX_UNITS: usize = 60;
/// `"Untitled artifact"` — [lib/store.js:196]
const DEFAULT_TITLE: &str = "Untitled artifact";
/// `org || "default"` — [lib/store.js:193]
const DEFAULT_ORG: &str = "default";

/// `normalizeCategory(value)` = `String(value || "").trim().replace(/\s+/g, " ").slice(0, 60)`
/// — [lib/store.js:57-59]
#[must_use]
pub fn normalize_category(value: &str) -> String {
    let trimmed = js_trim(value);
    let mut collapsed = String::with_capacity(trimmed.len());
    let mut in_whitespace = false;
    for character in trimmed.chars() {
        if is_js_whitespace(character) {
            if !in_whitespace {
                collapsed.push(' ');
                in_whitespace = true;
            }
        } else {
            collapsed.push(character);
            in_whitespace = false;
        }
    }
    js_slice(&collapsed, CATEGORY_MAX_UNITS).to_owned()
}

/// `String(title || "Untitled artifact").slice(0, 200)` — [lib/store.js:196]
fn normalize_title(value: Option<&str>) -> String {
    let value = value
        .filter(|value| !value.is_empty())
        .unwrap_or(DEFAULT_TITLE);
    js_slice(value, TITLE_MAX_UNITS).to_owned()
}

/// `String(description || "").slice(0, 500)` — [lib/store.js:197]
fn normalize_description(value: Option<&str>) -> String {
    js_slice(value.unwrap_or_default(), DESCRIPTION_MAX_UNITS).to_owned()
}

/// `String(uploaderLabel || "").slice(0, 60)` — [lib/store.js:195]
fn normalize_label(value: &str) -> String {
    js_slice(value, LABEL_MAX_UNITS).to_owned()
}

// ---------------------------------------------------------------------------
// SQL
// ---------------------------------------------------------------------------

/// Every v21 `artifacts` column, in the order [`meta_from_row`] reads them.
const META_COLUMNS: &str = "id, client_id, org, title, description, bytes, created_at, updated_at, \
     uploader_label, owner_email, is_bundle, entry, revision, category, hidden, body_sha256";

/// Columns of `artifact_revisions`, in the order [`revision_from_row`] reads them.
const REVISION_COLUMNS: &str = "artifact_id, org, revision, title, description, category, bytes, \
     is_bundle, entry, body_sha256, created_at, client_id";

fn u64_column(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<u64> {
    let value: i64 = row.get(index)?;
    u64::try_from(value).map_err(|_| rusqlite::Error::IntegralValueOutOfRange(index, value))
}

fn meta_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ArtifactMeta> {
    Ok(ArtifactMeta {
        id: ArtifactId(row.get(0)?),
        client_id: ClientId(row.get(1)?),
        org: OrgId(row.get(2)?),
        title: row.get(3)?,
        description: row.get(4)?,
        bytes: u64_column(row, 5)?,
        created_at: Timestamp(row.get(6)?),
        updated_at: Timestamp(row.get(7)?),
        uploader_label: row.get(8)?,
        owner_email: row.get(9)?,
        is_bundle: row.get::<_, i64>(10)? != 0,
        entry: row.get(11)?,
        revision: u64_column(row, 12)?,
        category: row.get(13)?,
        hidden: row.get::<_, i64>(14)? != 0,
        body_sha256: row.get(15)?,
    })
}

fn revision_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ArtifactRevision> {
    Ok(ArtifactRevision {
        artifact_id: ArtifactId(row.get(0)?),
        org: OrgId(row.get(1)?),
        revision: u64_column(row, 2)?,
        title: row.get(3)?,
        description: row.get(4)?,
        category: row.get(5)?,
        bytes: u64_column(row, 6)?,
        is_bundle: row.get::<_, i64>(7)? != 0,
        entry: row.get(8)?,
        body_sha256: row.get(9)?,
        created_at: Timestamp(row.get(10)?),
        client_id: row.get::<_, Option<String>>(11)?.map(ClientId),
    })
}

pub(crate) fn sql_error(operation: &'static str, error: &rusqlite::Error) -> AppError {
    tracing::error!(operation, error = %error, "artifact lifecycle sql failed");
    AppError::Internal
}

fn as_i64(value: u64) -> Result<i64, AppError> {
    i64::try_from(value).map_err(|_| AppError::Internal)
}

/// `getMetaStmt.get(id)` — [lib/store.js:111]
fn load_meta(conn: &Connection, id: &str) -> Result<Option<ArtifactMeta>, AppError> {
    conn.query_row(
        &format!("SELECT {META_COLUMNS} FROM artifacts WHERE id = ?1"),
        params![id],
        meta_from_row,
    )
    .optional()
    .map_err(|error| sql_error("load artifact metadata", &error))
}

fn list_metas(
    conn: &Connection,
    sql: &str,
    parameters: &[&dyn rusqlite::ToSql],
) -> Result<Vec<ArtifactMeta>, AppError> {
    let mut statement = conn
        .prepare(sql)
        .map_err(|error| sql_error("prepare artifact list", &error))?;
    let rows = statement
        .query_map(parameters, meta_from_row)
        .map_err(|error| sql_error("query artifact list", &error))?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|error| sql_error("read artifact list", &error))
}

/// Persist attribution and metadata for one produced revision.
fn record_revision_row(
    conn: &Connection,
    meta: &ArtifactMeta,
    acting_client_id: Option<&ClientId>,
) -> Result<(), AppError> {
    conn
        .execute(
            "INSERT OR REPLACE INTO artifact_revisions \
             (artifact_id, org, revision, title, description, category, bytes, is_bundle, entry, body_sha256, client_id) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                meta.id.0,
                meta.org.0,
                as_i64(meta.revision)?,
                meta.title,
                meta.description,
                meta.category,
                as_i64(meta.bytes)?,
                i64::from(meta.is_bundle),
                meta.entry,
                meta.body_sha256,
                acting_client_id.map(|client_id| &client_id.0),
            ],
        )
        .map_err(|error| sql_error("record revision row", &error))?;
    Ok(())
}

/// Preserve a pre-v22 live revision without falsely assigning it to the next editing key.
fn record_legacy_revision_row(conn: &Connection, meta: &ArtifactMeta) -> Result<(), AppError> {
    conn.execute(
        "INSERT OR IGNORE INTO artifact_revisions \
         (artifact_id, org, revision, title, description, category, bytes, is_bundle, entry, body_sha256, client_id) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, NULL)",
        params![
            meta.id.0,
            meta.org.0,
            as_i64(meta.revision)?,
            meta.title,
            meta.description,
            meta.category,
            as_i64(meta.bytes)?,
            i64::from(meta.is_bundle),
            meta.entry,
            meta.body_sha256,
        ],
    )
    .map_err(|error| sql_error("record legacy revision row", &error))?;
    Ok(())
}

/// `getRevisionStmt.get(id, revision)` — [lib/store.js:163-165]
fn load_revision(
    conn: &Connection,
    id: &str,
    revision: u64,
) -> Result<Option<ArtifactRevision>, AppError> {
    conn.query_row(
        &format!(
            "SELECT {REVISION_COLUMNS} FROM artifact_revisions \
             WHERE artifact_id = ?1 AND revision = ?2 \
               AND revision < (SELECT revision FROM artifacts WHERE id = ?1)"
        ),
        params![id, as_i64(revision)?],
        revision_from_row,
    )
    .optional()
    .map_err(|error| sql_error("load revision row", &error))
}

/// `orgExists(name)` = `SELECT 1 FROM orgs WHERE name = ?` — [lib/orgs.js:17], [lib/orgs.js:53-55]
///
/// U09 owns `persistence/orgs.rs` and has not landed yet; this is the single read-only statement
/// `store.js` needs, and it moves to U09's adapter when that unit lands.
fn org_exists(conn: &Connection, name: &str) -> Result<bool, AppError> {
    conn.query_row("SELECT 1 FROM orgs WHERE name = ?1", params![name], |_| {
        Ok(())
    })
    .optional()
    .map(|found: Option<()>| found.is_some())
    .map_err(|error| sql_error("check organization", &error))
}

// ---------------------------------------------------------------------------
// Store
// ---------------------------------------------------------------------------

/// Everything a lifecycle closure needs, owned so it can move into `spawn_blocking`.
#[derive(Debug)]
pub struct StoreContext {
    artifact_dir: PathBuf,
    limits: StorageLimits,
    ids: Arc<dyn IdSource>,
    faults: Arc<dyn FaultInjector>,
}

/// The SQLite + filesystem [`ArtifactService`], mirroring `createArtifactStore`.
/// [lib/store.js:92-102]
#[derive(Debug, Clone)]
pub struct ArtifactStore {
    pool: DbPool,
    context: Arc<StoreContext>,
}

impl ArtifactStore {
    /// `files.mkdirSync(artifactDir, { recursive: true })` runs here, as Node does at store
    /// construction. [lib/store.js:103]
    #[must_use]
    pub fn new(
        pool: DbPool,
        artifact_dir: PathBuf,
        limits: StorageLimits,
        ids: Arc<dyn IdSource>,
    ) -> Self {
        Self::with_faults(pool, artifact_dir, limits, ids, Arc::new(NoFaults))
    }

    /// The failpoint-aware constructor used by the crash matrix.
    #[must_use]
    pub fn with_faults(
        pool: DbPool,
        artifact_dir: PathBuf,
        limits: StorageLimits,
        ids: Arc<dyn IdSource>,
        faults: Arc<dyn FaultInjector>,
    ) -> Self {
        // Best effort, exactly like Node: a store over an unwritable directory fails per
        // operation rather than at construction.
        let _ = std::fs::create_dir_all(&artifact_dir);
        Self {
            pool,
            context: Arc::new(StoreContext {
                artifact_dir,
                limits,
                ids,
                faults,
            }),
        }
    }

    /// Compose the store from the validated application configuration.
    #[must_use]
    pub fn from_config(pool: DbPool, config: &AppConfig, ids: Arc<dyn IdSource>) -> Self {
        Self::new(pool, config.artifact_dir(), config.storage, ids)
    }

    /// The directory every body path is derived from.
    #[must_use]
    pub fn artifact_dir(&self) -> &Path {
        &self.context.artifact_dir
    }

    fn context(&self) -> Arc<StoreContext> {
        Arc::clone(&self.context)
    }
}

/// Staged state for `publish`'s compensation. [lib/store.js:222-227]
#[derive(Debug)]
struct PublishState {
    inserted: bool,
}

/// The body relocation `delete` may have to undo. [lib/store.js:503-514]
#[derive(Debug)]
struct MovedBody {
    source: PathBuf,
    trash: PathBuf,
}

/// The outgoing body a successful `update` parked in `.history`. [lib/store.js:549-561]
#[derive(Debug)]
struct BodySnapshot {
    source: PathBuf,
    destination: PathBuf,
    moved: bool,
}

/// The pre-update metadata `update` reverts to when the swap fails. [lib/store.js:394-404]
#[derive(Debug, Clone)]
struct MetaBefore {
    title: String,
    description: String,
    bytes: u64,
    entry: String,
    category: String,
    body_sha256: String,
    revision: u64,
    updated_at: String,
}

/// Everything `publish_attempt` needs about the body it is installing.
struct PublishPlan<'a> {
    id: &'a SafeArtifactId,
    staging: &'a Path,
    final_path: &'a Path,
    bytes: u64,
    body_sha256: &'a str,
    is_bundle: bool,
    entry: &'a str,
    single: Option<&'a str>,
    bundle: Option<&'a ValidatedBundle>,
}

/// The replacement body an update stages, if any.
#[derive(Debug)]
enum StagedBody {
    None,
    Single(String),
    Bundle(Vec<(String, String)>),
}

/// The resolved shape of an update, computed before any side effect. [lib/store.js:331-372]
#[derive(Debug)]
struct UpdatePlan {
    title: String,
    description: String,
    category: String,
    bytes: u64,
    entry: String,
    body_sha256: String,
    body: StagedBody,
    content_changed: bool,
    changed: bool,
}

impl StoreContext {
    fn fault(&self, point: FaultPoint) -> Result<(), Interrupt> {
        self.faults.check(point).map_err(Interrupt::from)
    }

    /// A fault at a boundary that sits outside any compensating `try`.
    fn bare_fault(&self, point: FaultPoint) -> Result<(), AppError> {
        self.faults.check(point).map_err(InjectedFault::into_error)
    }

    fn single_body_path(&self, id: &SafeArtifactId) -> PathBuf {
        paths::single_body_path(&self.artifact_dir, id)
    }

    fn bundle_dir(&self, id: &SafeArtifactId) -> PathBuf {
        paths::bundle_dir(&self.artifact_dir, id)
    }

    fn body_path(&self, id: &SafeArtifactId, is_bundle: bool) -> PathBuf {
        paths::body_path(&self.artifact_dir, id, is_bundle)
    }

    /// `transientPath(id, kind)` — the random suffix is a fresh artifact-alphabet nanoid.
    /// [lib/store.js:178-180]
    fn transient_path(
        &self,
        id: &SafeArtifactId,
        kind: TransientKind,
    ) -> Result<PathBuf, AppError> {
        let token = self.ids.artifact_id()?;
        paths::transient_path(&self.artifact_dir, id, kind, &token.0).ok_or(AppError::Internal)
    }

    /// `nextId()` — [lib/store.js:182-188]
    ///
    /// Node loops forever; a bounded retry keeps a broken id source from hanging a request. The
    /// chance of 64 consecutive collisions over a 33^12 space is not physically meaningful.
    fn next_id(&self, conn: &Connection) -> Result<SafeArtifactId, AppError> {
        const MAX_ATTEMPTS: usize = 64;
        for _ in 0..MAX_ATTEMPTS {
            let candidate = self.ids.artifact_id()?;
            if is_reserved_artifact_id(&candidate.0) {
                continue;
            }
            let Some(id) = SafeArtifactId::parse(&candidate.0) else {
                continue;
            };
            if path_exists(&self.single_body_path(&id)) || path_exists(&self.bundle_dir(&id)) {
                continue;
            }
            if load_meta(conn, id.as_str())?.is_some() {
                continue;
            }
            return Ok(id);
        }
        tracing::error!("exhausted artifact id attempts");
        Err(AppError::Internal)
    }

    /// `files.writeFileSync(staging, html, "utf8")` — [lib/store.js:217], [lib/store.js:376]
    fn stage_single_body(&self, staging: &Path, html: &str) -> Result<(), AppError> {
        if let Some(parent) = staging.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| io_failure("create staging parent", staging, &error))?;
        }
        std::fs::write(staging, html.as_bytes())
            .map_err(|error| io_failure("write staged body", staging, &error))
    }

    /// ```text
    /// for (const [rel, content] of clean) {
    ///   const full = path.join(staging, rel);
    ///   files.mkdirSync(path.dirname(full), { recursive: true });
    ///   files.writeFileSync(full, content, "utf8");
    /// }
    /// ```
    /// — [lib/store.js:263-267], [lib/store.js:379-383]
    ///
    /// Files are written in the caller's order — the order the publisher supplied, and the order
    /// entry auto-selection already used (contract delta 4). Nothing here re-sorts them, and a
    /// duplicate relative path keeps Node's "last write wins".
    ///
    /// See contract-delta request 2 for the trailing-separator rejection.
    fn stage_bundle_files(
        &self,
        staging: &Path,
        files: &[(String, String)],
    ) -> Result<(), AppError> {
        std::fs::create_dir_all(staging)
            .map_err(|error| io_failure("create staging directory", staging, &error))?;
        for (rel, content) in files {
            if rel.ends_with('/') {
                // Node's `writeFileSync` on a trailing-slash path fails with EISDIR; `Path::join`
                // would silently drop the separator and create a file instead.
                tracing::error!(path = %rel, "bundle file name resolves to a directory");
                return Err(AppError::Internal);
            }
            let Some(full) = validation::contained_path(staging, rel) else {
                tracing::error!(path = %rel, "bundle file escaped its staging root");
                return Err(AppError::Internal);
            };
            if let Some(parent) = full.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|error| io_failure("create bundle directory", parent, &error))?;
            }
            std::fs::write(&full, content.as_bytes())
                .map_err(|error| io_failure("write bundle file", &full, &error))?;
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // publish
    // -----------------------------------------------------------------------

    /// `publish` / `publishBundle` — [lib/store.js:206-281]
    fn publish_sync(
        &self,
        conn: &Connection,
        request: PublishArtifact,
    ) -> Result<PublishedArtifact, AppError> {
        let prepared = self.prepare_publish(&request.content)?;

        let id = self.next_id(conn)?;
        let staging = self.transient_path(&id, TransientKind::Staging)?;
        let final_path = self.body_path(&id, prepared.is_bundle);

        let mut state = PublishState { inserted: false };
        let attempt = self.publish_attempt(
            conn,
            &request,
            PublishPlan {
                id: &id,
                staging: &staging,
                final_path: &final_path,
                bytes: prepared.bytes,
                body_sha256: &prepared.body_sha256,
                is_bundle: prepared.is_bundle,
                entry: &prepared.entry,
                single: prepared.single.as_deref(),
                bundle: prepared.bundle.as_ref(),
            },
            &mut state,
        );

        match attempt {
            Ok(meta) => Ok(PublishedArtifact {
                meta,
                file_count: prepared.file_count,
            }),
            Err(interrupt) => {
                if interrupt.compensate {
                    // `if (inserted) deleteById.run(id); safeRemove(staging); safeRemove(final);`
                    // — [lib/store.js:222-227], [lib/store.js:275-280]
                    if state.inserted {
                        self.bare_fault(FaultPoint::PublishDeleteRow)?;
                        conn.execute("DELETE FROM artifacts WHERE id = ?1", params![id.as_str()])
                            .map_err(|error| sql_error("delete published artifact", &error))?;
                    }
                    safe_remove(&staging);
                    safe_remove(&final_path);
                }
                Err(interrupt.error)
            }
        }
    }

    /// Validation and digesting, before any side effect. [lib/store.js:207-211],
    /// [lib/store.js:231-260]
    fn prepare_publish(&self, content: &ArtifactContent) -> Result<PreparedPublish, AppError> {
        match content {
            ArtifactContent::SingleHtml(html) => {
                let validated = validation::validate_single_body(html, &self.limits)?;
                Ok(PreparedPublish {
                    bytes: validated.bytes,
                    body_sha256: validated.body_sha256,
                    is_bundle: false,
                    entry: String::new(),
                    file_count: None,
                    single: Some(html.clone()),
                    bundle: None,
                })
            }
            ArtifactContent::Bundle { files, entry } => {
                // `publishBundle` passes no `preferEntry`, so auto-selection takes the FIRST
                // `.html` in publisher order. [lib/store.js:252-255]
                let validated =
                    validation::validate_bundle(files, entry.as_deref(), None, &self.limits)?;
                let body_sha256 = bundle_manifest_digest(&validated.files);
                Ok(PreparedPublish {
                    bytes: validated.total_bytes,
                    body_sha256,
                    is_bundle: true,
                    entry: validated.entry.clone(),
                    file_count: Some(validated.files.len()),
                    single: None,
                    bundle: Some(validated),
                })
            }
        }
    }

    fn publish_attempt(
        &self,
        conn: &Connection,
        request: &PublishArtifact,
        plan: PublishPlan<'_>,
        state: &mut PublishState,
    ) -> Result<ArtifactMeta, Interrupt> {
        self.fault(FaultPoint::PublishStageWrite)?;
        if let Some(html) = plan.single {
            self.stage_single_body(plan.staging, html)?;
        } else if let Some(bundle) = plan.bundle {
            self.stage_bundle_files(plan.staging, &bundle.files)?;
        }

        self.fault(FaultPoint::PublishInsert)?;
        let org = if request.target_org.0.is_empty() {
            DEFAULT_ORG
        } else {
            request.target_org.0.as_str()
        };
        // Ownership is server-controlled.  The MCP request contains no owner field; read the
        // current authenticated key row while publishing and snapshot that nullable identity.
        // A later key reassignment therefore cannot transfer historical artifacts.
        let owner_email: Option<String> = conn
            .query_row(
                "SELECT owner_email FROM api_keys WHERE client_id = ?1 AND org = ?2",
                params![request.publisher.client_id.0, request.publisher.org.0],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| Interrupt::from(sql_error("read publisher key owner", &error)))?
            .flatten();
        conn.execute(
            "INSERT INTO artifacts \
             (id, client_id, org, owner_email, uploader_label, title, description, bytes, is_bundle, entry, \
              category, body_sha256) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                plan.id.as_str(),
                request.publisher.client_id.0,
                org,
                owner_email,
                normalize_label(&request.publisher.label),
                normalize_title(request.title.as_deref()),
                normalize_description(request.description.as_deref()),
                as_i64(plan.bytes)?,
                i64::from(plan.is_bundle),
                plan.entry,
                normalize_category(request.category.as_deref().unwrap_or_default()),
                plan.body_sha256,
            ],
        )
        .map_err(|error| Interrupt::from(sql_error("insert artifact", &error)))?;
        state.inserted = true;

        let meta = load_meta(conn, plan.id.as_str())?
            .ok_or_else(|| Interrupt::from(AppError::Internal))?;
        record_revision_row(conn, &meta, Some(&request.publisher.client_id))?;

        self.fault(FaultPoint::PublishRename)?;
        rename(plan.staging, plan.final_path)?;

        self.fault(FaultPoint::PublishComplete)?;
        Ok(meta)
    }

    // -----------------------------------------------------------------------
    // update
    // -----------------------------------------------------------------------

    /// `update({ … })` — [lib/store.js:314-442]
    ///
    /// `expected_revision` is already resolved by the caller: Node substitutes `meta.revision`
    /// when the client omitted it ([lib/store.js:318], [lib/mcp.js:407]), and the frozen
    /// [`ArtifactUpdate`] has no "absent" state.
    fn update_sync(
        &self,
        conn: &mut Connection,
        id: &str,
        expected_revision: u64,
        update: &ArtifactUpdate,
    ) -> Result<UpdateArtifactResult, AppError> {
        let Some(meta) = load_meta(conn, id)? else {
            return Err(AppError::NotFound("not_found".to_owned()));
        };
        let safe_id = SafeArtifactId::parse(id).ok_or(AppError::ConcealedNotFound)?;

        // `if (!Number.isInteger(g) || g < 1 || g !== meta.revision) return conflict;`
        // — [lib/store.js:319-321]
        if expected_revision < 1 || expected_revision != meta.revision {
            return Err(AppError::Conflict("conflict".to_owned()));
        }

        let plan = self.plan_update(&safe_id, &meta, update)?;
        if !plan.changed {
            // An exact no-op creates NO revision, no history snapshot, and no replacement body.
            // [lib/store.js:365-370]
            return Ok(UpdateArtifactResult {
                meta,
                changed: false,
            });
        }

        // Stage the replacement before touching the database. [lib/store.js:374-384]
        let mut staged: Option<PathBuf> = None;
        if plan.content_changed {
            self.bare_fault(FaultPoint::UpdateStageWrite)?;
            let path = self.transient_path(&safe_id, TransientKind::Staging)?;
            let staging_result = match &plan.body {
                StagedBody::Single(html) => self.stage_single_body(&path, html),
                StagedBody::Bundle(files) => self.stage_bundle_files(&path, files),
                StagedBody::None => Ok(()),
            };
            if let Err(error) = staging_result {
                safe_remove(&path);
                return Err(error);
            }
            staged = Some(path);
        }

        let before = MetaBefore {
            title: meta.title.clone(),
            description: meta.description.clone(),
            bytes: meta.bytes,
            entry: meta.entry.clone(),
            category: meta.category.clone(),
            body_sha256: meta.body_sha256.clone(),
            revision: meta.revision,
            updated_at: meta.updated_at.0.clone(),
        };

        if let Err(fault) = self.faults.check(FaultPoint::UpdateCommit) {
            let interrupt = Interrupt::from(fault);
            if interrupt.compensate
                && let Some(path) = &staged
            {
                safe_remove(path);
            }
            return Err(interrupt.error);
        }

        // Commit metadata FIRST, then swap the body. [lib/store.js:386-393]
        let committed = self.commit_update(
            conn,
            &meta,
            expected_revision,
            &plan,
            update.acting_client_id.as_ref(),
        )?;
        if !committed {
            // `if (!committed) { if (staged) safeRemove(staged); return conflict; }`
            // — [lib/store.js:416-419]
            if let Some(path) = &staged {
                safe_remove(path);
            }
            return Err(AppError::Conflict("conflict".to_owned()));
        }

        let mut snapshot: Option<BodySnapshot> = None;
        if let Err(interrupt) = self.swap_body(&safe_id, &meta, staged.as_deref(), &mut snapshot) {
            if interrupt.compensate {
                // Node's catch, in order: restore the snapshot, drop the staged body, revert the
                // committed metadata and its revision row. [lib/store.js:429-437]
                self.restore_snapshot_body(snapshot.as_ref())?;
                if let Some(path) = &staged {
                    safe_remove(path);
                }
                self.revert_update(conn, &meta, &before)?;
            }
            return Err(interrupt.error);
        }

        // Pruning is best-effort in Node too; a fault here leaves a fully committed update.
        self.bare_fault(FaultPoint::UpdatePrune)?;
        self.prune_history(conn, &safe_id);

        let meta = load_meta(conn, id)?.ok_or(AppError::Internal)?;
        Ok(UpdateArtifactResult {
            meta,
            changed: true,
        })
    }

    /// Validation, digesting, and change detection — everything before the first side effect.
    /// [lib/store.js:323-372]
    fn plan_update(
        &self,
        id: &SafeArtifactId,
        meta: &ArtifactMeta,
        update: &ArtifactUpdate,
    ) -> Result<UpdatePlan, AppError> {
        let wants_single = matches!(update.content, Some(ArtifactContent::SingleHtml(_)));
        // Contract-delta request 1: an empty file list plus an entry is Node's "entry only"
        // update; an empty file list on its own still reaches `files is empty`.
        let entry_only = matches!(
            &update.content,
            Some(ArtifactContent::Bundle { files, entry }) if files.is_empty() && entry.is_some()
        );
        let wants_bundle =
            matches!(update.content, Some(ArtifactContent::Bundle { .. })) && !entry_only;

        if wants_single && meta.is_bundle {
            return Err(AppError::Validation(
                "artifact is a bundle; pass files, not html".to_owned(),
            ));
        }
        if wants_bundle && !meta.is_bundle {
            return Err(AppError::Validation(
                "artifact is single-file; pass html, not files".to_owned(),
            ));
        }
        if entry_only && !meta.is_bundle {
            return Err(AppError::Validation(
                "artifact is single-file; entry only applies to bundles".to_owned(),
            ));
        }

        let next_title = update
            .title
            .as_deref()
            .map_or_else(|| meta.title.clone(), |value| normalize_title(Some(value)));
        let next_description = update.description.as_deref().map_or_else(
            || meta.description.clone(),
            |value| normalize_description(Some(value)),
        );
        let next_category = update
            .category
            .as_deref()
            .map_or_else(|| meta.category.clone(), normalize_category);

        let mut next_bytes = meta.bytes;
        let mut next_entry = meta.entry.clone();
        let mut next_body_sha256 = meta.body_sha256.clone();
        let mut body = StagedBody::None;

        match &update.content {
            Some(ArtifactContent::SingleHtml(html)) => {
                let validated = validation::validate_single_body(html, &self.limits)?;
                next_bytes = validated.bytes;
                next_body_sha256 = validated.body_sha256;
                body = StagedBody::Single(html.clone());
            }
            Some(ArtifactContent::Bundle { files, entry }) if wants_bundle => {
                let built = validation::validate_bundle(
                    files,
                    entry.as_deref(),
                    Some(&meta.entry),
                    &self.limits,
                )?;
                next_bytes = built.total_bytes;
                next_entry = built.entry.clone();
                next_body_sha256 = bundle_manifest_digest(&built.files);
                body = StagedBody::Bundle(built.files);
            }
            Some(ArtifactContent::Bundle { entry, .. }) => {
                // Entry-only: the directory stays live and is snapshotted by copy.
                // [lib/store.js:354-360]
                let requested = entry.as_deref().unwrap_or_default();
                let selected = if requested.is_empty() {
                    Some(meta.entry.clone())
                } else {
                    sanitize_relative_path(requested)
                };
                let usable = selected.as_ref().is_some_and(|value| {
                    !value.is_empty()
                        && read::read_bundle_file(&self.artifact_dir, id, value, None).is_some()
                });
                let Some(selected) = selected.filter(|_| usable) else {
                    return Err(AppError::Validation(format!(
                        "entry \"{requested}\" is not one of the files"
                    )));
                };
                next_entry = selected;
            }
            None => {}
        }

        // [lib/store.js:362-364]
        let content_changed = next_bytes != meta.bytes || next_body_sha256 != meta.body_sha256;
        let changed = content_changed
            || next_entry != meta.entry
            || next_title != meta.title
            || next_description != meta.description
            || next_category != meta.category;

        Ok(UpdatePlan {
            title: next_title,
            description: next_description,
            category: next_category,
            bytes: next_bytes,
            entry: next_entry,
            body_sha256: next_body_sha256,
            body: if content_changed {
                body
            } else {
                StagedBody::None
            },
            content_changed,
            changed,
        })
    }

    /// The metadata transaction: guarded UPDATE plus the OUTGOING revision row, atomically.
    /// [lib/store.js:406-415]
    ///
    /// The guard pins `client_id` and `org` to the values read at the start of the operation, so
    /// a concurrent re-tenant or delete turns into a conflict instead of a lost update.
    fn commit_update(
        &self,
        conn: &mut Connection,
        meta: &ArtifactMeta,
        expected_revision: u64,
        plan: &UpdatePlan,
        acting_client_id: Option<&ClientId>,
    ) -> Result<bool, AppError> {
        let transaction = conn
            .transaction()
            .map_err(|error| sql_error("open update transaction", &error))?;
        let changes = transaction
            .execute(
                "UPDATE artifacts \
                 SET title = ?1, description = ?2, bytes = ?3, entry = ?4, category = ?5, \
                     body_sha256 = ?6, revision = revision + 1, updated_at = datetime('now') \
                 WHERE id = ?7 AND client_id = ?8 AND org = ?9 AND revision = ?10",
                params![
                    plan.title,
                    plan.description,
                    as_i64(plan.bytes)?,
                    plan.entry,
                    plan.category,
                    plan.body_sha256,
                    meta.id.0,
                    meta.client_id.0,
                    meta.org.0,
                    as_i64(expected_revision)?,
                ],
            )
            .map_err(|error| sql_error("update artifact metadata", &error))?;
        if changes != 1 {
            // `if (info.changes !== 1) return false;` — dropping the transaction rolls it back.
            return Ok(false);
        }
        record_legacy_revision_row(&transaction, meta)?;
        let mut produced = meta.clone();
        produced.title.clone_from(&plan.title);
        produced.description.clone_from(&plan.description);
        produced.bytes = plan.bytes;
        produced.entry.clone_from(&plan.entry);
        produced.category.clone_from(&plan.category);
        produced.body_sha256.clone_from(&plan.body_sha256);
        produced.revision = meta.revision.saturating_add(1);
        record_revision_row(&transaction, &produced, acting_client_id)?;
        // A fault here drops the transaction unread, which is exactly what a crash before COMMIT
        // leaves behind.
        self.bare_fault(FaultPoint::UpdateCommitTransaction)?;
        transaction
            .commit()
            .map_err(|error| sql_error("commit update transaction", &error))?;
        Ok(true)
    }

    /// `snapshotBody` then the staged rename. [lib/store.js:420-428]
    fn swap_body(
        &self,
        id: &SafeArtifactId,
        meta: &ArtifactMeta,
        staged: Option<&Path>,
        snapshot: &mut Option<BodySnapshot>,
    ) -> Result<(), Interrupt> {
        self.fault(FaultPoint::UpdateSnapshot)?;
        *snapshot = self.snapshot_body(id, meta, staged.is_some())?;

        self.fault(FaultPoint::UpdateSwap)?;
        if let Some(path) = staged {
            rename(path, &self.body_path(id, meta.is_bundle))?;
        }
        Ok(())
    }

    /// `snapshotBody(id, meta, { moveBody })` — [lib/store.js:549-561]
    ///
    /// `moveBody` relocates the live body (freeing the final path for the replacement);
    /// otherwise it is copied, because a metadata-only update keeps its body live.
    fn snapshot_body(
        &self,
        id: &SafeArtifactId,
        meta: &ArtifactMeta,
        move_body: bool,
    ) -> Result<Option<BodySnapshot>, AppError> {
        let source = self.body_path(id, meta.is_bundle);
        if !path_exists(&source) {
            return Ok(None);
        }
        let destination =
            paths::history_body_path(&self.artifact_dir, id, meta.revision, meta.is_bundle);
        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| io_failure("create history directory", parent, &error))?;
        }
        safe_remove(&destination);
        if move_body {
            rename(&source, &destination)?;
            return Ok(Some(BodySnapshot {
                source,
                destination,
                moved: true,
            }));
        }
        copy_recursive(&source, &destination)
            .map_err(|error| io_failure("copy history snapshot", &destination, &error))?;
        Ok(Some(BodySnapshot {
            source,
            destination,
            moved: false,
        }))
    }

    /// `restoreSnapshotBody(snap)` — only a *moved* snapshot is undone. [lib/store.js:562-565]
    fn restore_snapshot_body(&self, snapshot: Option<&BodySnapshot>) -> Result<(), AppError> {
        self.bare_fault(FaultPoint::UpdateRestoreSnapshot)?;
        let Some(snapshot) = snapshot else {
            return Ok(());
        };
        if !snapshot.moved || !path_exists(&snapshot.destination) {
            return Ok(());
        }
        rename(&snapshot.destination, &snapshot.source)
    }

    /// `deleteRevisionStmt.run(id, meta.revision); restoreMetaStmt.run(before);` in one
    /// transaction. [lib/store.js:432-435]
    fn revert_update(
        &self,
        conn: &mut Connection,
        meta: &ArtifactMeta,
        before: &MetaBefore,
    ) -> Result<(), AppError> {
        self.bare_fault(FaultPoint::UpdateRevertMetadata)?;
        let transaction = conn
            .transaction()
            .map_err(|error| sql_error("open revert transaction", &error))?;
        transaction
            .execute(
                "DELETE FROM artifact_revisions WHERE artifact_id = ?1 AND revision = ?2",
                params![meta.id.0, as_i64(before.revision.saturating_add(1))?],
            )
            .map_err(|error| sql_error("delete revision row", &error))?;
        transaction
            .execute(
                "UPDATE artifacts \
                 SET title = ?1, description = ?2, bytes = ?3, entry = ?4, category = ?5, \
                     body_sha256 = ?6, revision = ?7, updated_at = ?8 \
                 WHERE id = ?9",
                params![
                    before.title,
                    before.description,
                    as_i64(before.bytes)?,
                    before.entry,
                    before.category,
                    before.body_sha256,
                    as_i64(before.revision)?,
                    before.updated_at,
                    meta.id.0,
                ],
            )
            .map_err(|error| sql_error("revert artifact metadata", &error))?;
        transaction
            .commit()
            .map_err(|error| sql_error("commit revert transaction", &error))
    }

    /// `pruneHistory(id)` — best effort, exactly like Node's swallowing `try`.
    /// [lib/store.js:568-575]
    fn prune_history(&self, conn: &Connection, id: &SafeArtifactId) {
        let Ok(offset) = i64::try_from(self.limits.max_history) else {
            return;
        };
        let Ok(mut statement) = conn.prepare(
            "SELECT revision, is_bundle FROM artifact_revisions \
             WHERE artifact_id = ?1 \
               AND revision < (SELECT revision FROM artifacts WHERE id = ?1) \
             ORDER BY revision DESC LIMIT -1 OFFSET ?2",
        ) else {
            return;
        };
        let prunable = statement
            .query_map(params![id.as_str(), offset], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)? != 0))
            })
            .and_then(std::iter::Iterator::collect::<rusqlite::Result<Vec<_>>>);
        let Ok(prunable) = prunable else {
            return;
        };
        for (revision, is_bundle) in prunable {
            let _ = conn.execute(
                "DELETE FROM artifact_revisions WHERE artifact_id = ?1 AND revision = ?2",
                params![id.as_str(), revision],
            );
            let Ok(revision) = u64::try_from(revision) else {
                continue;
            };
            safe_remove(&paths::history_body_path(
                &self.artifact_dir,
                id,
                revision,
                is_bundle,
            ));
        }
    }

    // -----------------------------------------------------------------------
    // restore
    // -----------------------------------------------------------------------

    /// `restoreById(id, revision, …)` — replay a past revision as a NEW revision.
    /// [lib/store.js:605-624]
    ///
    /// `update` snapshots the *current* revision first, so a restore is itself undoable.
    fn restore_sync(
        &self,
        conn: &mut Connection,
        id: &str,
        revision: u64,
        acting_client_id: Option<ClientId>,
    ) -> Result<RestoreArtifactResult, AppError> {
        let Some(meta) = load_meta(conn, id)? else {
            return Err(AppError::NotFound("not_found".to_owned()));
        };
        let safe_id = SafeArtifactId::parse(id).ok_or(AppError::ConcealedNotFound)?;
        let Some(row) = load_revision(conn, id, revision)? else {
            return Err(AppError::NotFound("revision_not_found".to_owned()));
        };
        if row.is_bundle != meta.is_bundle {
            return Err(AppError::Conflict("type_mismatch".to_owned()));
        }
        let body_path =
            paths::history_body_path(&self.artifact_dir, &safe_id, row.revision, row.is_bundle);
        if !path_exists(&body_path) {
            return Err(AppError::Gone("body_missing".to_owned()));
        }

        let content = if row.is_bundle {
            // `readTree` returns the snapshot and the entry is supplied explicitly, so the map's
            // ordering never reaches entry auto-selection. [lib/store.js:617-618]
            let files = read::read_tree(&body_path)?.into_iter().collect();
            ArtifactContent::Bundle {
                files,
                entry: Some(row.entry.clone()),
            }
        } else {
            let html = std::fs::read(&body_path)
                .map_err(|error| io_failure("read history body", &body_path, &error))?;
            ArtifactContent::SingleHtml(String::from_utf8_lossy(&html).into_owned())
        };

        let update = ArtifactUpdate {
            expected_revision: meta.revision,
            acting_client_id,
            title: Some(row.title.clone()),
            description: Some(row.description.clone()),
            category: Some(row.category.clone()),
            content: Some(content),
        };
        let result = self.update_sync(conn, id, meta.revision, &update)?;
        Ok(RestoreArtifactResult {
            meta: result.meta,
            restored_from: row.revision,
        })
    }

    // -----------------------------------------------------------------------
    // delete
    // -----------------------------------------------------------------------

    /// `removeById(id, …)` — [lib/store.js:626-644]
    fn delete_sync(&self, conn: &Connection, id: &str) -> Result<bool, AppError> {
        let Some(meta) = load_meta(conn, id)? else {
            return Ok(false);
        };
        let safe_id = SafeArtifactId::parse(id).ok_or(AppError::ConcealedNotFound)?;

        // `moveBodyToTrash` runs OUTSIDE the try: a failure here has nothing to compensate.
        // [lib/store.js:503-509], [lib/store.js:630]
        let moved = self.move_body_to_trash(&safe_id, &meta)?;

        match self.delete_attempt(conn, &safe_id, moved.as_ref()) {
            Ok(deleted) => Ok(deleted),
            Err(interrupt) => {
                if interrupt.compensate {
                    self.restore_body(moved.as_ref())?;
                }
                Err(interrupt.error)
            }
        }
    }

    fn delete_attempt(
        &self,
        conn: &Connection,
        id: &SafeArtifactId,
        moved: Option<&MovedBody>,
    ) -> Result<bool, Interrupt> {
        self.fault(FaultPoint::DeleteRow)?;
        // Reactions cascade on `artifacts(id)`; feedback, revisions, views, and shares cascade on
        // the composite `artifacts(id, org)`. [lib/migrations.js:124,167,261,299,357]
        let changes = conn
            .execute("DELETE FROM artifacts WHERE id = ?1", params![id.as_str()])
            .map_err(|error| Interrupt::from(sql_error("delete artifact", &error)))?;
        if changes == 0 {
            // `restoreBody(moved); return false;` — [lib/store.js:633-636]
            self.restore_body(moved)?;
            return Ok(false);
        }

        self.fault(FaultPoint::DeleteTrashRemove)?;
        if let Some(moved) = moved {
            safe_remove(&moved.trash);
        }

        self.fault(FaultPoint::DeleteHistoryRemove)?;
        // Revision rows cascade through the composite FK; this removes their bodies.
        // [lib/store.js:638]
        safe_remove(&paths::history_dir(&self.artifact_dir, id));
        Ok(true)
    }

    /// `moveBodyToTrash(id, meta)` — [lib/store.js:503-509]
    fn move_body_to_trash(
        &self,
        id: &SafeArtifactId,
        meta: &ArtifactMeta,
    ) -> Result<Option<MovedBody>, AppError> {
        let source = self.body_path(id, meta.is_bundle);
        if !path_exists(&source) {
            return Ok(None);
        }
        self.bare_fault(FaultPoint::DeleteTrashRename)?;
        let trash = self.transient_path(id, TransientKind::Trash)?;
        rename(&source, &trash)?;
        Ok(Some(MovedBody { source, trash }))
    }

    /// `restoreBody(moved)` — [lib/store.js:511-514]
    fn restore_body(&self, moved: Option<&MovedBody>) -> Result<(), AppError> {
        self.bare_fault(FaultPoint::DeleteRestoreBody)?;
        let Some(moved) = moved else {
            return Ok(());
        };
        if !path_exists(&moved.trash) {
            return Ok(());
        }
        rename(&moved.trash, &moved.source)
    }

    // -----------------------------------------------------------------------
    // metadata-only mutations
    // -----------------------------------------------------------------------

    /// `setCategory(id, category)` — [lib/store.js:446-452]
    fn set_category_sync(
        &self,
        conn: &Connection,
        id: &str,
        category: &str,
    ) -> Result<ArtifactMeta, AppError> {
        if load_meta(conn, id)?.is_none() {
            return Err(AppError::NotFound("not_found".to_owned()));
        }
        self.bare_fault(FaultPoint::MetadataWrite)?;
        conn.execute(
            "UPDATE artifacts SET category = ?1, updated_at = datetime('now') WHERE id = ?2",
            params![normalize_category(category), id],
        )
        .map_err(|error| sql_error("set artifact category", &error))?;
        load_meta(conn, id)?.ok_or(AppError::Internal)
    }

    /// `setHidden(id, hidden)` — hidden is an unlisted flag, never an access boundary.
    /// [lib/store.js:455-461]
    fn set_hidden_sync(
        &self,
        conn: &Connection,
        id: &str,
        hidden: bool,
    ) -> Result<ArtifactMeta, AppError> {
        if load_meta(conn, id)?.is_none() {
            return Err(AppError::NotFound("not_found".to_owned()));
        }
        self.bare_fault(FaultPoint::MetadataWrite)?;
        conn.execute(
            "UPDATE artifacts SET hidden = ?1, updated_at = datetime('now') WHERE id = ?2",
            params![i64::from(hidden), id],
        )
        .map_err(|error| sql_error("set artifact visibility", &error))?;
        load_meta(conn, id)?.ok_or(AppError::Internal)
    }

    /// `moveArtifactToOrg(id, targetOrg, category)` — [lib/store.js:463-480]
    ///
    /// One transaction with deferred foreign keys so the parent and every org-bearing child move
    /// together; public shares are REVOKED rather than carried into the new tenant, and
    /// `client_id` deliberately stays put so the old org-locked key can no longer update it.
    fn move_to_org_sync(
        &self,
        conn: &mut Connection,
        id: &str,
        target_org: &str,
        category: Option<&str>,
    ) -> Result<ArtifactMeta, AppError> {
        let Some(meta) = load_meta(conn, id)? else {
            return Err(AppError::NotFound("not_found".to_owned()));
        };
        let org = js_trim(target_org).to_owned();
        if !org_exists(conn, &org)? {
            return Err(AppError::Validation(format!(
                "Unknown organization \"{org}\"."
            )));
        }
        let next_category = category.map_or_else(|| meta.category.clone(), normalize_category);

        self.bare_fault(FaultPoint::MoveTransaction)?;
        let transaction = conn
            .transaction()
            .map_err(|error| sql_error("open move transaction", &error))?;
        // Composite FKs are checked at COMMIT, after the parent and all children have moved.
        transaction
            .execute_batch("PRAGMA defer_foreign_keys = ON")
            .map_err(|error| sql_error("defer foreign keys", &error))?;
        transaction
            .execute(
                "UPDATE artifacts SET org = ?1, category = ?2, updated_at = datetime('now') \
                 WHERE id = ?3",
                params![org, next_category, id],
            )
            .map_err(|error| sql_error("move artifact", &error))?;
        for (sql, operation) in [
            (
                "UPDATE feedback SET org = ?1 WHERE artifact_id = ?2",
                "move feedback",
            ),
            (
                "UPDATE artifact_revisions SET org = ?1 WHERE artifact_id = ?2",
                "move revisions",
            ),
            (
                "UPDATE artifact_views SET org = ?1 WHERE artifact_id = ?2",
                "move views",
            ),
        ] {
            transaction
                .execute(sql, params![org, id])
                .map_err(|error| sql_error(operation, &error))?;
        }
        transaction
            .execute(
                "DELETE FROM artifact_shares WHERE artifact_id = ?1",
                params![id],
            )
            .map_err(|error| sql_error("revoke shares on move", &error))?;
        transaction
            .commit()
            .map_err(|error| sql_error("commit move transaction", &error))?;

        load_meta(conn, id)?.ok_or(AppError::Internal)
    }

    /// `listRevisions(id)` — [lib/store.js:577-581]
    fn list_revisions_sync(
        &self,
        conn: &Connection,
        meta: &ArtifactMeta,
    ) -> Result<RevisionHistory, AppError> {
        // Node reloads the live row before listing. Use that revision both for the response and
        // to exclude the live attribution marker from the retained-history array.
        let current = load_meta(conn, &meta.id.0)?.map_or(meta.revision, |row| row.revision);
        let mut statement = conn
            .prepare(&format!(
                "SELECT {REVISION_COLUMNS} FROM artifact_revisions \
                 WHERE artifact_id = ?1 AND revision < ?2 ORDER BY revision DESC"
            ))
            .map_err(|error| sql_error("prepare revision list", &error))?;
        let revisions = statement
            .query_map(params![meta.id.0, as_i64(current)?], revision_from_row)
            .map_err(|error| sql_error("query revision list", &error))?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|error| sql_error("read revision list", &error))?;
        Ok(RevisionHistory { current, revisions })
    }
}

/// A validated publish request: the body is digested and the entry resolved, nothing is written.
struct PreparedPublish {
    bytes: u64,
    body_sha256: String,
    is_bundle: bool,
    entry: String,
    file_count: Option<usize>,
    single: Option<String>,
    bundle: Option<ValidatedBundle>,
}

// ---------------------------------------------------------------------------
// Metadata-addressed operations
// ---------------------------------------------------------------------------

/// The store's own API takes the artifact's [`ArtifactMeta`], not an [`AuthorizedArtifact`].
///
/// That mirrors the reference exactly — `lib/store.js` performs no authorization ("Authorization
/// is the caller's responsibility (route uses artifactAccess)", [lib/store.js:445]) — and it keeps
/// the wrapper what the contract says it is: a *caller-side* proof consumed by the frozen
/// [`ArtifactService`] surface below, which is the only path routes and MCP tools use. It also
/// lets U08's crash matrix drive every mutation before U06 lands the wrapper constructors.
impl ArtifactStore {
    /// The body half of `readArtifact(id)` — [lib/store.js:494-501]
    ///
    /// # Errors
    /// Returns [`AppError::Internal`] when the blocking task fails.
    pub async fn read_body_for(
        &self,
        meta: &ArtifactMeta,
    ) -> Result<Option<ArtifactFile>, AppError> {
        let context = self.context();
        let id = meta.id.0.clone();
        blocking(move || {
            // `readArtifact` refuses a reserved id before touching the filesystem.
            // [lib/store.js:495]
            let Some(id) = SafeArtifactId::addressable(&id) else {
                return Ok(None);
            };
            Ok(read::read_body(&context.artifact_dir, &id))
        })
        .await
    }

    /// `readBundleFile(id, relPath)` — [lib/store.js:482-492]
    ///
    /// # Errors
    /// Returns [`AppError::Internal`] when the blocking task fails.
    pub async fn read_bundle_file_for(
        &self,
        meta: &ArtifactMeta,
        relative_path: &str,
    ) -> Result<Option<ArtifactFile>, AppError> {
        let context = self.context();
        let meta = meta.clone();
        let relative_path = relative_path.to_owned();
        blocking(move || {
            // `readBundleFile` returns null for a single-file artifact. [lib/store.js:484]
            if !meta.is_bundle {
                return Ok(None);
            }
            let Some(id) = SafeArtifactId::parse(&meta.id.0) else {
                return Ok(None);
            };
            Ok(read::read_bundle_file(
                &context.artifact_dir,
                &id,
                &meta.entry,
                Some(relative_path.as_str()),
            ))
        })
        .await
    }

    /// `readHistoryArtifact` (no path) versus `readHistoryBundleFile` (a path, possibly empty,
    /// which falls back to the revision's own entry).
    /// [lib/store.js:583-601], [lib/app.js:434], [lib/app.js:445]
    ///
    /// # Errors
    /// Propagates a database failure as [`AppError::Internal`].
    pub async fn read_revision_body_for(
        &self,
        meta: &ArtifactMeta,
        revision: u64,
        relative_path: Option<&str>,
    ) -> Result<Option<ArtifactFile>, AppError> {
        let context = self.context();
        let id = meta.id.0.clone();
        let relative_path = relative_path.map(ToOwned::to_owned);
        db::interact(&self.pool, move |conn| {
            let Some(safe_id) = SafeArtifactId::parse(&id) else {
                return Ok(None);
            };
            let Some(row) = load_revision(conn, &id, revision)? else {
                return Ok(None);
            };
            Ok(match relative_path {
                None if row.is_bundle => None,
                None => read::read_revision_body(&context.artifact_dir, &safe_id, revision),
                Some(_) if !row.is_bundle => None,
                Some(relative) => read::read_revision_bundle_file(
                    &context.artifact_dir,
                    &safe_id,
                    revision,
                    &row.entry,
                    Some(relative.as_str()),
                ),
            })
        })
        .await
    }

    pub async fn list_bundle_files_for(
        &self,
        meta: &ArtifactMeta,
        revision: Option<u64>,
    ) -> Result<Option<Vec<(String, u64)>>, AppError> {
        let context = self.context();
        let id = meta.id.0.clone();
        let is_bundle = meta.is_bundle;
        db::interact(&self.pool, move |conn| {
            let Some(safe_id) = SafeArtifactId::parse(&id) else {
                return Ok(None);
            };
            let root = if let Some(revision) = revision {
                let Some(row) = load_revision(conn, &id, revision)? else {
                    return Ok(None);
                };
                if !row.is_bundle {
                    return Ok(None);
                }
                paths::history_body_path(&context.artifact_dir, &safe_id, revision, true)
            } else {
                if !is_bundle {
                    return Ok(None);
                }
                paths::bundle_dir(&context.artifact_dir, &safe_id)
            };
            read::list_bundle_files(&root)
        })
        .await
    }

    /// `listRevisions(id)` — [lib/store.js:577-581]
    ///
    /// # Errors
    /// Propagates a database failure as [`AppError::Internal`].
    pub async fn list_revisions_for(
        &self,
        meta: &ArtifactMeta,
    ) -> Result<RevisionHistory, AppError> {
        let context = self.context();
        let meta = meta.clone();
        db::interact(&self.pool, move |conn| {
            context.list_revisions_sync(conn, &meta)
        })
        .await
    }

    /// `update({ … })` — [lib/store.js:314-442]
    ///
    /// # Errors
    /// [`AppError::NotFound`] (`not_found`), [`AppError::Conflict`] (`conflict`),
    /// [`AppError::Validation`] with Node's message, or a propagated I/O failure.
    pub async fn update_for(
        &self,
        meta: &ArtifactMeta,
        update: ArtifactUpdate,
    ) -> Result<UpdateArtifactResult, AppError> {
        let context = self.context();
        let id = meta.id.0.clone();
        db::interact(&self.pool, move |conn| {
            context.update_sync(conn, &id, update.expected_revision, &update)
        })
        .await
    }

    /// `restoreById(id, revision, …)` — [lib/store.js:605-624]
    ///
    /// # Errors
    /// [`AppError::NotFound`] (`not_found` / `revision_not_found`), [`AppError::Conflict`]
    /// (`type_mismatch`), or [`AppError::Gone`] (`body_missing`).
    pub async fn restore_for(
        &self,
        meta: &ArtifactMeta,
        revision: u64,
        acting_client_id: Option<ClientId>,
    ) -> Result<RestoreArtifactResult, AppError> {
        let context = self.context();
        let id = meta.id.0.clone();
        db::interact(&self.pool, move |conn| {
            context.restore_sync(conn, &id, revision, acting_client_id)
        })
        .await
    }

    /// `removeById(id, …)` — [lib/store.js:626-644]
    ///
    /// # Errors
    /// Propagates a database or filesystem failure after compensating.
    pub async fn delete_for(&self, meta: &ArtifactMeta) -> Result<bool, AppError> {
        let context = self.context();
        let id = meta.id.0.clone();
        db::interact(&self.pool, move |conn| context.delete_sync(conn, &id)).await
    }

    /// `setCategory(id, category)` — [lib/store.js:446-452]
    ///
    /// # Errors
    /// [`AppError::NotFound`] (`not_found`) or a propagated database failure.
    pub async fn set_category_for(
        &self,
        meta: &ArtifactMeta,
        category: &str,
    ) -> Result<ArtifactMeta, AppError> {
        let context = self.context();
        let id = meta.id.0.clone();
        let category = category.to_owned();
        db::interact(&self.pool, move |conn| {
            context.set_category_sync(conn, &id, &category)
        })
        .await
    }

    /// `setHidden(id, hidden)` — [lib/store.js:455-461]
    ///
    /// # Errors
    /// [`AppError::NotFound`] (`not_found`) or a propagated database failure.
    pub async fn set_hidden_for(
        &self,
        meta: &ArtifactMeta,
        hidden: bool,
    ) -> Result<ArtifactMeta, AppError> {
        let context = self.context();
        let id = meta.id.0.clone();
        db::interact(&self.pool, move |conn| {
            context.set_hidden_sync(conn, &id, hidden)
        })
        .await
    }

    /// `moveArtifactToOrg(id, targetOrg, category)` — [lib/store.js:463-480]
    ///
    /// # Errors
    /// [`AppError::NotFound`] (`not_found`) or [`AppError::Validation`] with Node's
    /// `Unknown organization "…".` message.
    pub async fn move_to_org_for(
        &self,
        meta: &ArtifactMeta,
        target_org: &str,
        category: Option<&str>,
    ) -> Result<ArtifactMeta, AppError> {
        let context = self.context();
        let id = meta.id.0.clone();
        let target_org = target_org.to_owned();
        let category = category.map(ToOwned::to_owned);
        db::interact(&self.pool, move |conn| {
            context.move_to_org_sync(conn, &id, &target_org, category.as_deref())
        })
        .await
    }
}

// ---------------------------------------------------------------------------
// ArtifactService
// ---------------------------------------------------------------------------

impl ArtifactService for ArtifactStore {
    fn find_meta<'a>(
        &'a self,
        id: &'a ArtifactId,
    ) -> BoxFuture<'a, Result<Option<ArtifactMeta>, AppError>> {
        let id = id.0.clone();
        Box::pin(async move {
            // `getArtifactMeta: (id) => isReserved(id) ? null : getMetaStmt.get(id) || null`
            // — [lib/store.js:793]
            if is_reserved_artifact_id(&id) {
                return Ok(None);
            }
            db::interact(&self.pool, move |conn| load_meta(conn, &id)).await
        })
    }

    fn publish(
        &self,
        request: PublishArtifact,
    ) -> BoxFuture<'_, Result<PublishedArtifact, AppError>> {
        let context = self.context();
        Box::pin(async move {
            db::interact(&self.pool, move |conn| context.publish_sync(conn, request)).await
        })
    }

    fn list_for_publisher<'a>(
        &'a self,
        publisher: &'a PublisherIdentity,
    ) -> BoxFuture<'a, Result<Vec<ArtifactMeta>, AppError>> {
        let client_id = publisher.client_id.0.clone();
        let org = (!publisher.is_admin()).then(|| publisher.org.0.clone());
        Box::pin(async move {
            db::interact(&self.pool, move |conn| match org {
                // `listByClient` — [lib/store.js:112]
                None => list_metas(
                    conn,
                    &format!(
                        "SELECT {META_COLUMNS} FROM artifacts WHERE client_id = ?1 \
                         ORDER BY created_at DESC"
                    ),
                    &[&client_id],
                ),
                // `listByClientOrg` — an org-locked key must never keep listing artifacts moved
                // to another tenant. [lib/store.js:115]
                Some(org) => list_metas(
                    conn,
                    &format!(
                        "SELECT {META_COLUMNS} FROM artifacts WHERE client_id = ?1 AND org = ?2 \
                         ORDER BY created_at DESC"
                    ),
                    &[&client_id, &org],
                ),
            })
            .await
        })
    }

    fn list_org_artifacts<'a>(
        &'a self,
        org: &'a OrgId,
        include_hidden: bool,
    ) -> BoxFuture<'a, Result<Vec<ArtifactMeta>, AppError>> {
        let org = org.0.clone();
        Box::pin(async move {
            db::interact(&self.pool, move |conn| {
                // `listByOrg` / `listByOrgIncludingHidden` — [lib/store.js:116-117]
                let filter = if include_hidden {
                    ""
                } else {
                    " AND hidden = 0"
                };
                list_metas(
                    conn,
                    &format!(
                        "SELECT {META_COLUMNS} FROM artifacts WHERE org = ?1{filter} \
                         ORDER BY client_id ASC, created_at DESC"
                    ),
                    &[&org],
                )
            })
            .await
        })
    }

    fn list_all_grouped_by_org(
        &self,
        include_hidden: bool,
    ) -> BoxFuture<'_, Result<Vec<OrgArtifacts>, AppError>> {
        Box::pin(async move {
            db::interact(&self.pool, move |conn| {
                // `listAll` / `listAllVisible`, grouped by org in first-seen order.
                // [lib/store.js:118-119], [lib/store.js:61-69]
                let filter = if include_hidden {
                    ""
                } else {
                    " WHERE hidden = 0"
                };
                let rows = list_metas(
                    conn,
                    &format!(
                        "SELECT {META_COLUMNS} FROM artifacts{filter} \
                         ORDER BY org ASC, client_id ASC, created_at DESC"
                    ),
                    &[],
                )?;
                let mut grouped: Vec<OrgArtifacts> = Vec::new();
                for row in rows {
                    match grouped.last_mut() {
                        Some(group) if group.org == row.org => group.items.push(row),
                        _ => grouped.push(OrgArtifacts {
                            org: row.org.clone(),
                            items: vec![row],
                        }),
                    }
                }
                Ok(grouped)
            })
            .await
        })
    }

    fn list_org_ids<'a>(
        &'a self,
        org: &'a OrgId,
        include_hidden: bool,
    ) -> BoxFuture<'a, Result<Vec<ArtifactId>, AppError>> {
        let org = org.0.clone();
        Box::pin(async move {
            db::interact(&self.pool, move |conn| {
                // `listIdsByOrg` / `listIdsByOrgIncludingHidden` — [lib/store.js:109-110]
                let filter = if include_hidden {
                    ""
                } else {
                    " AND hidden = 0"
                };
                let mut statement = conn
                    .prepare(&format!(
                        "SELECT id FROM artifacts WHERE org = ?1{filter} \
                         ORDER BY created_at DESC, id DESC"
                    ))
                    .map_err(|error| sql_error("prepare org id list", &error))?;
                statement
                    .query_map(params![org], |row| row.get::<_, String>(0).map(ArtifactId))
                    .map_err(|error| sql_error("query org id list", &error))?
                    .collect::<rusqlite::Result<Vec<_>>>()
                    .map_err(|error| sql_error("read org id list", &error))
            })
            .await
        })
    }

    fn read_body<'a>(
        &'a self,
        artifact: &'a AuthorizedArtifact,
    ) -> BoxFuture<'a, Result<Option<ArtifactFile>, AppError>> {
        Box::pin(self.read_body_for(artifact.meta()))
    }

    fn read_bundle_file<'a>(
        &'a self,
        artifact: &'a AuthorizedArtifact,
        relative_path: &'a str,
    ) -> BoxFuture<'a, Result<Option<ArtifactFile>, AppError>> {
        Box::pin(self.read_bundle_file_for(artifact.meta(), relative_path))
    }

    fn read_revision_body<'a>(
        &'a self,
        artifact: &'a AuthorizedArtifact,
        revision: u64,
        relative_path: Option<&'a str>,
    ) -> BoxFuture<'a, Result<Option<ArtifactFile>, AppError>> {
        Box::pin(self.read_revision_body_for(artifact.meta(), revision, relative_path))
    }

    fn list_bundle_files<'a>(
        &'a self,
        artifact: &'a AuthorizedArtifact,
        revision: Option<u64>,
    ) -> BoxFuture<'a, Result<Option<Vec<(String, u64)>>, AppError>> {
        Box::pin(self.list_bundle_files_for(artifact.meta(), revision))
    }

    fn list_revisions<'a>(
        &'a self,
        artifact: &'a AuthorizedArtifact,
    ) -> BoxFuture<'a, Result<RevisionHistory, AppError>> {
        Box::pin(self.list_revisions_for(artifact.meta()))
    }

    fn update(
        &self,
        artifact: AuthorizedArtifact,
        update: ArtifactUpdate,
    ) -> BoxFuture<'_, Result<UpdateArtifactResult, AppError>> {
        Box::pin(async move { self.update_for(artifact.meta(), update).await })
    }

    fn restore(
        &self,
        artifact: AuthorizedArtifact,
        revision: u64,
        acting_client_id: Option<ClientId>,
    ) -> BoxFuture<'_, Result<RestoreArtifactResult, AppError>> {
        Box::pin(async move {
            self.restore_for(artifact.meta(), revision, acting_client_id)
                .await
        })
    }

    fn delete(&self, artifact: AuthorizedArtifact) -> BoxFuture<'_, Result<bool, AppError>> {
        Box::pin(async move { self.delete_for(artifact.meta()).await })
    }

    fn set_category(
        &self,
        artifact: AuthorizedArtifact,
        category: String,
    ) -> BoxFuture<'_, Result<ArtifactMeta, AppError>> {
        Box::pin(async move { self.set_category_for(artifact.meta(), &category).await })
    }

    fn set_hidden(
        &self,
        artifact: AuthorizedArtifact,
        hidden: bool,
    ) -> BoxFuture<'_, Result<ArtifactMeta, AppError>> {
        Box::pin(async move { self.set_hidden_for(artifact.meta(), hidden).await })
    }

    fn move_to_org(
        &self,
        artifact: AuthorizedArtifact,
        target_org: OrgId,
        category: Option<String>,
    ) -> BoxFuture<'_, Result<ArtifactMeta, AppError>> {
        Box::pin(async move {
            self.move_to_org_for(artifact.meta(), &target_org.0, category.as_deref())
                .await
        })
    }

    fn audit_storage(
        &self,
        clean_transient: bool,
    ) -> BoxFuture<'_, Result<StorageAuditReport, AppError>> {
        let context = self.context();
        Box::pin(async move {
            db::interact(&self.pool, move |conn| {
                reconciliation::audit_storage(
                    conn,
                    &context.artifact_dir,
                    clean_transient,
                    context.faults.as_ref(),
                )
            })
            .await
        })
    }

    fn backfill_body_digests(&self) -> BoxFuture<'_, Result<DigestBackfillReport, AppError>> {
        let context = self.context();
        Box::pin(async move {
            db::interact(&self.pool, move |conn| {
                reconciliation::backfill_body_digests(
                    conn,
                    &context.artifact_dir,
                    context.faults.as_ref(),
                )
            })
            .await
        })
    }
}

/// Filesystem-only work still has to leave the async worker threads.
async fn blocking<T, F>(work: F) -> Result<T, AppError>
where
    F: FnOnce() -> Result<T, AppError> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(work).await.map_err(|error| {
        tracing::error!(error = %error, "artifact blocking task failed");
        AppError::Internal
    })?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slices_by_utf16_code_units_like_javascript() {
        assert_eq!(js_slice("abcdef", 3), "abc");
        assert_eq!(js_slice("abc", 10), "abc");
        // "é" is one UTF-16 unit but two UTF-8 bytes.
        assert_eq!(js_slice("éé", 1), "é");
        // An astral character is two UTF-16 units; a limit of one drops it entirely rather than
        // producing a lone surrogate.
        assert_eq!(js_slice("\u{1f600}x", 1), "");
        assert_eq!(js_slice("\u{1f600}x", 2), "\u{1f600}");
    }

    #[test]
    fn normalizes_categories_like_node() {
        assert_eq!(normalize_category("  design   docs \n"), "design docs");
        assert_eq!(normalize_category(""), "");
        assert_eq!(normalize_category("\u{feff}a\u{a0}b"), "a b");
        assert_eq!(normalize_category(&"x".repeat(100)).len(), 60);
    }

    #[test]
    fn applies_nodes_metadata_defaults() {
        assert_eq!(normalize_title(None), "Untitled artifact");
        assert_eq!(normalize_title(Some("")), "Untitled artifact");
        assert_eq!(normalize_title(Some("Real")), "Real");
        assert_eq!(normalize_description(None), "");
        assert_eq!(normalize_label(&"l".repeat(80)).len(), 60);
    }

    #[test]
    fn scripted_faults_fire_once_and_record_visits() {
        let faults = ScriptedFaults::new().fail_once(FaultPoint::PublishRename);
        assert!(faults.check(FaultPoint::PublishInsert).is_ok());
        assert!(matches!(
            faults.check(FaultPoint::PublishRename),
            Err(InjectedFault::Error(_))
        ));
        assert!(faults.check(FaultPoint::PublishRename).is_ok());
        assert!(faults.all_fired());
        assert_eq!(
            faults.visited(),
            vec![
                FaultPoint::PublishInsert,
                FaultPoint::PublishRename,
                FaultPoint::PublishRename
            ]
        );
    }

    #[test]
    fn crash_faults_skip_compensation() {
        let faults = ScriptedFaults::new().crash_once(FaultPoint::PublishRename);
        let fault = faults
            .check(FaultPoint::PublishRename)
            .expect_err("armed fault fires");
        assert!(matches!(fault, InjectedFault::Crash(_)));
        assert!(!Interrupt::from(fault).compensate);
        assert!(NoFaults.check(FaultPoint::DeleteRow).is_ok());
    }
}
