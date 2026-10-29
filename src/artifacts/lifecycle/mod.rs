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
//!    separator and create a *file*. `StoreContext::stage_bundle_files` therefore refuses such
//!    a name so both runtimes fail the publish. Fixing it properly means rejecting the name in
//!    `sanitizeRel` on both sides.
//! 3. **Lifecycle failures carry Node's `reason` token as their message** (`not_found`,
//!    `conflict`, `revision_not_found`, `type_mismatch`, `body_missing`). The HTTP restore route
//!    returns that token verbatim ([lib/app.js:718-719]) while the MCP tool renders a friendly
//!    sentence ([lib/mcp.js:517-523]), so the token is the only shape both consumers can derive
//!    theirs from. The `AppError` variant already encodes the status Node uses.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};

use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};

use crate::config::{AppConfig, Clock, IdSource, StorageLimits, SystemClock};
use crate::error::AppError;
use crate::integrations::delivery_envelope::{
    DeliveryEnvelopeV1, DeliveryPreviewReferenceV1, stable_delivery_event_id,
};
use crate::model::{
    ArtifactContent, ArtifactFile, ArtifactId, ArtifactMeta, ArtifactRevision, ArtifactUpdate,
    ClientId, DigestBackfillReport, NotificationPayload, OrgArtifacts, OrgId, PublishArtifact,
    PublishedArtifact, PublisherIdentity, RestoreArtifactResult, RevisionHistory,
    StorageAuditReport, Timestamp, UpdateArtifactResult, WebhookEvent,
};
use crate::persistence::db::{self, DbPool};
use crate::persistence::{outbox, outbox_fanout};
use crate::ports::{ArtifactService, BoxFuture};
use crate::security::access::AuthorizedArtifact;
use crate::security::audit::{
    AuditEvent, MutationAudit, append_in_transaction, delete_pending_receipt_in_transaction,
    finalize_receipt_in_transaction, initialize_head, parse_hmac_key,
    reserve_receipt_in_transaction, verify, verify_pending_receipt_in_transaction,
};

use super::digest::{body_digest_at_path, bundle_manifest_digest};
use super::durable;
use super::paths::{self, SafeArtifactId, TransientKind};
use super::read;
use super::reconciliation;
use super::validation::{
    self, ValidatedBundle, is_reserved_artifact_id, js_trim, sanitize_relative_path,
};

mod api;
mod body_swap;
mod context;
mod faults;
mod fs;
mod metadata;
mod normalize;
mod org_move;
mod publish;
mod service;
mod sql;
mod trash;
mod update;

pub use faults::{FaultInjector, FaultPoint, InjectedFault, NoFaults, ScriptedFaults};
pub(crate) use fs::{copy_recursive, io_failure, path_exists, rename, safe_remove};
pub use normalize::normalize_category;
pub use sql::ARTIFACT_KEY_TAKEN;
pub(crate) use sql::sql_error;

use faults::Interrupt;
use normalize::*;
use sql::*;

// ---------------------------------------------------------------------------
// Store
// ---------------------------------------------------------------------------

/// Everything a lifecycle closure needs, owned so it can move into `spawn_blocking`.
#[derive(Debug)]
pub struct StoreContext {
    artifact_dir: PathBuf,
    delivery_base_url: String,
    limits: StorageLimits,
    ids: Arc<dyn IdSource>,
    faults: Arc<dyn FaultInjector>,
    // Non-optional: a configured writer must never silently become an unaudited writer.
    audit_key: [u8; 32],
    /// Linearizes body/history reads against lifecycle mutations. Authorization remains at the
    /// route boundary; this lock only ensures an already-authorized read cannot interleave a
    /// ready-to-pending transition and then touch a different body generation.
    lifecycle_gate: RwLock<()>,
}

/// The SQLite + filesystem [`ArtifactService`], mirroring `createArtifactStore`.
/// [lib/store.js:92-102]
#[derive(Debug, Clone)]
pub struct ArtifactStore {
    pool: DbPool,
    context: Arc<StoreContext>,
    preview_scheduler: Option<Arc<dyn PostCommitPreviewScheduler>>,
}

/// Schedules persistent gallery-thumbnail preparation after a successful lifecycle commit.
///
/// This is deliberately separate from provider delivery: no subscriber lookup, webhook secret,
/// or network send can determine whether an uploaded artifact receives its gallery preview.
pub trait PostCommitPreviewScheduler: Send + Sync + fmt::Debug {
    /// Must return immediately after bounded queue admission; it must not read a body or render
    /// while the mutation is completing.
    fn schedule(&self, store: ArtifactStore, meta: ArtifactMeta);
}

impl ArtifactStore {
    /// Compatibility constructor for direct adapters. Production construction receives the
    /// configured public base URL through the configuration constructor.
    #[must_use]
    pub fn new(
        pool: DbPool,
        artifact_dir: PathBuf,
        limits: StorageLimits,
        ids: Arc<dyn IdSource>,
        audit_key: [u8; 32],
    ) -> Self {
        Self::new_for_test(pool, artifact_dir, limits, ids, audit_key)
    }

    /// `files.mkdirSync(artifactDir, { recursive: true })` runs here, as Node does at store
    /// construction. [lib/store.js:103]
    #[must_use]
    pub fn new_for_test(
        pool: DbPool,
        artifact_dir: PathBuf,
        limits: StorageLimits,
        ids: Arc<dyn IdSource>,
        audit_key: [u8; 32],
    ) -> Self {
        Self::with_faults_for_test(
            pool,
            artifact_dir,
            limits,
            ids,
            Arc::new(NoFaults),
            audit_key,
        )
    }

    /// The failpoint-aware constructor used by the crash matrix.
    #[must_use]
    pub fn with_faults_for_test(
        pool: DbPool,
        artifact_dir: PathBuf,
        limits: StorageLimits,
        ids: Arc<dyn IdSource>,
        faults: Arc<dyn FaultInjector>,
        audit_key: [u8; 32],
    ) -> Self {
        Self::with_delivery_base_url(
            pool,
            artifact_dir,
            limits,
            ids,
            faults,
            audit_key,
            "https://artifact.test".to_owned(),
        )
    }

    /// Compatibility failpoint constructor. It remains an explicit test seam.
    #[must_use]
    pub fn with_faults(
        pool: DbPool,
        artifact_dir: PathBuf,
        limits: StorageLimits,
        ids: Arc<dyn IdSource>,
        faults: Arc<dyn FaultInjector>,
        audit_key: [u8; 32],
    ) -> Self {
        Self::with_faults_for_test(pool, artifact_dir, limits, ids, faults, audit_key)
    }

    fn with_delivery_base_url(
        pool: DbPool,
        artifact_dir: PathBuf,
        limits: StorageLimits,
        ids: Arc<dyn IdSource>,
        faults: Arc<dyn FaultInjector>,
        audit_key: [u8; 32],
        delivery_base_url: String,
    ) -> Self {
        // Best effort, exactly like Node: a store over an unwritable directory fails per
        // operation rather than at construction.
        let _ = std::fs::create_dir_all(&artifact_dir);
        Self {
            pool,
            context: Arc::new(StoreContext {
                artifact_dir,
                delivery_base_url,
                limits,
                ids,
                faults,
                audit_key,
                lifecycle_gate: RwLock::new(()),
            }),
            preview_scheduler: None,
        }
    }

    /// Compose the store from the validated application configuration.
    pub fn from_config(
        pool: DbPool,
        config: &AppConfig,
        ids: Arc<dyn IdSource>,
    ) -> Result<Self, AppError> {
        let encoded = config.audit_ledger_hmac_key.as_ref().ok_or_else(|| {
            AppError::Validation(
                "AUDIT_LEDGER_HMAC_KEY is required; refusing to start without a tamper-evident audit ledger"
                    .to_owned(),
            )
        })?;
        let store = Self::with_delivery_base_url(
            pool,
            config.artifact_dir(),
            config.storage,
            ids,
            Arc::new(NoFaults),
            parse_hmac_key(encoded.expose())?,
            config.public_base_url.clone(),
        );
        // Validate the anchor before startup reconciliation can perform any recovery mutation.
        // `initialize_head` seals a pristine migration head exactly once; any pre-existing
        // wrong key, rollback, or altered event fails closed.
        let conn = db::checkout(&store.pool)?;
        initialize_head(&conn, &store.context.audit_key)?;
        if !verify(&conn, &store.context.audit_key)? {
            crate::observability::record_global_security_signal("integrity_failure");
            return Err(AppError::Validation(
                "security audit ledger integrity verification failed; refusing to start".to_owned(),
            ));
        }
        Ok(store)
    }

    /// The directory every body path is derived from.
    #[must_use]
    pub fn artifact_dir(&self) -> &Path {
        &self.context.artifact_dir
    }

    /// Adds a production-only post-commit thumbnail scheduler. Test constructors deliberately
    /// leave this unset, so lifecycle tests remain inert unless they opt in explicitly.
    #[must_use]
    pub fn with_post_commit_preview_scheduler(
        mut self,
        preview_scheduler: Arc<dyn PostCommitPreviewScheduler>,
    ) -> Self {
        self.preview_scheduler = Some(preview_scheduler);
        self
    }

    fn context(&self) -> Arc<StoreContext> {
        Arc::clone(&self.context)
    }

    fn schedule_preview(&self, meta: ArtifactMeta) {
        if let Some(scheduler) = &self.preview_scheduler {
            scheduler.schedule(self.clone(), meta);
        }
    }
}

/// Staged state for `publish`'s compensation. [lib/store.js:222-227]
#[derive(Debug)]
struct PublishState {
    inserted: bool,
    final_durable: bool,
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
