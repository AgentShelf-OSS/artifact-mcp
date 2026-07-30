//! U08 crash matrix: a fault is injected at **every** write, transaction, rename, snapshot,
//! delete, and compensation step, and the system must always land in a recoverable state.
//!
//! Two flavours are injected at each boundary:
//!
//! * [`ScriptedFaults::fail_once`] — an in-process error, so the operation's compensation runs
//!   exactly as Node's `catch` block does ([lib/store.js:222-227], [lib/store.js:429-437],
//!   [lib/store.js:640-643]).
//! * [`ScriptedFaults::crash_once`] — the process died at that boundary, so **nothing**
//!   compensates. The surviving on-disk and database state is then handed to
//!   `audit_storage(clean_transient = true)`, which is exactly what `server.js:89` runs at
//!   startup.
//!
//! Three invariants are asserted after every case:
//!
//! 1. **Never serve uncommitted content** — the installed body always matches the committed
//!    `body_sha256`, or the divergence is *reported* rather than silently served.
//! 2. **Never lose the prior or the current body** — after recovery the body is either the
//!    pre-operation content or the committed replacement, never absent and never truncated.
//! 3. **Startup converges** — a second audit is a no-op.

use std::sync::{
    Arc, Barrier,
    atomic::{AtomicBool, Ordering},
};

use artifact_mcp::artifacts::lifecycle::{
    FaultInjector, FaultPoint, InjectedFault, ScriptedFaults,
};
use artifact_mcp::error::AppError;
use artifact_mcp::model::{ArtifactContent, ArtifactId, ArtifactMeta, ArtifactUpdate, OrgId};
use artifact_mcp::ports::{
    ArtifactService as _, BoxFuture, PreviewService, integrations::PreviewPriority,
};
use artifact_mcp::security::access::AccessPolicy;

use crate::u08_support::{Fixture, TEST_ORG, html_update, mutation_audit, publisher, sha256_hex};

const OLD: &str = "<p>OLD</p>";
const NEW: &str = "<p>NEW-and-longer</p>";

/// Pauses exactly once at a production lifecycle boundary. Separate barriers make the test's
/// observation and release phases deterministic rather than scheduler-timing dependent.
#[derive(Debug)]
struct BlockingUpdateFault {
    armed: AtomicBool,
    entered: Arc<Barrier>,
    release: Arc<Barrier>,
}

impl BlockingUpdateFault {
    fn new() -> Self {
        Self {
            armed: AtomicBool::new(false),
            entered: Arc::new(Barrier::new(2)),
            release: Arc::new(Barrier::new(2)),
        }
    }

    fn arm(&self) {
        self.armed.store(true, Ordering::Release);
    }
}

impl FaultInjector for BlockingUpdateFault {
    fn check(&self, point: FaultPoint) -> Result<(), InjectedFault> {
        if point == FaultPoint::UpdateStageWrite && self.armed.swap(false, Ordering::AcqRel) {
            self.entered.wait();
            self.release.wait();
        }
        Ok(())
    }
}

/// A preview adapter that blocks only while ArtifactStore's production thumbnail port owns its
/// lifecycle read guard. Its byte payload is intentionally opaque: this proof is about the
/// check/read lifetime, not image decoding.
#[derive(Debug)]
struct BlockingPreview {
    armed: AtomicBool,
    entered: Arc<Barrier>,
    release: Arc<Barrier>,
}

impl BlockingPreview {
    fn new() -> Self {
        Self {
            armed: AtomicBool::new(false),
            entered: Arc::new(Barrier::new(2)),
            release: Arc::new(Barrier::new(2)),
        }
    }

    fn arm(&self) {
        self.armed.store(true, Ordering::Release);
    }
}

impl PreviewService for BlockingPreview {
    fn enabled(&self) -> bool {
        true
    }

    fn read_thumbnail<'a>(
        &'a self,
        _artifact: &'a artifact_mcp::security::access::AuthorizedArtifact,
        _digest: &'a str,
    ) -> BoxFuture<'a, Result<Option<Vec<u8>>, AppError>> {
        Box::pin(async { Ok(None) })
    }

    fn read_thumbnail_sync(
        &self,
        _meta: &ArtifactMeta,
        _digest: &str,
    ) -> Result<Option<Vec<u8>>, AppError> {
        if self.armed.swap(false, Ordering::AcqRel) {
            self.entered.wait();
            self.release.wait();
        }
        Ok(Some(b"preview bytes".to_vec()))
    }

    fn placeholder(&self, _meta: &ArtifactMeta, _accent: Option<&str>) -> Vec<u8> {
        Vec::new()
    }

    fn ensure_thumbnail<'a>(
        &'a self,
        _meta: &'a ArtifactMeta,
        _html: &'a str,
        _priority: PreviewPriority,
    ) -> BoxFuture<'a, Result<Option<Vec<u8>>, AppError>> {
        Box::pin(async { Ok(None) })
    }

    fn remove_artifact<'a>(&'a self, _id: &'a ArtifactId) -> BoxFuture<'a, Result<(), AppError>> {
        Box::pin(async { Ok(()) })
    }
}

/// Every artifact row in the fixture tenant, hidden included.
async fn artifacts(fixture: &Fixture) -> Vec<ArtifactMeta> {
    fixture
        .store
        .list_org_artifacts(&OrgId(TEST_ORG.to_owned()), true)
        .await
        .expect("list artifacts")
}

async fn sole_artifact(fixture: &Fixture) -> Option<ArtifactMeta> {
    artifacts(fixture).await.into_iter().next()
}

/// Invariant 1 + 2: whatever body is installed, the committed digest describes it exactly.
fn assert_body_matches_metadata(fixture: &Fixture, meta: &ArtifactMeta, context: &str) {
    let body = fixture
        .body_on_disk(meta)
        .unwrap_or_else(|| panic!("{context}: the body must exist after recovery"));
    assert_eq!(
        sha256_hex(&body),
        meta.body_sha256,
        "{context}: the installed body must match the committed digest"
    );
    assert!(
        body == OLD || body == NEW,
        "{context}: the body is neither the prior nor the committed content ({body:?})"
    );
}

/// Invariant 3: recovery converges — a second audit changes nothing.
async fn assert_audit_converges(fixture: &Fixture, context: &str) {
    let report = fixture
        .store
        .audit_storage(true)
        .await
        .unwrap_or_else(|error| panic!("{context}: second audit failed: {error}"));
    assert!(
        report.recovered_paths.is_empty() && report.transient_paths.is_empty(),
        "{context}: startup recovery is not idempotent: {report:?}"
    );
}

// ---------------------------------------------------------------------------
// publish
// ---------------------------------------------------------------------------

const PUBLISH_POINTS: [FaultPoint; 6] = [
    FaultPoint::PublishStageWrite,
    FaultPoint::PublishStageFileSync,
    FaultPoint::PublishStageDirectorySync,
    FaultPoint::PublishInsert,
    FaultPoint::PublishRename,
    FaultPoint::PublishComplete,
];

#[tokio::test]
async fn publish_compensation_erases_every_partial_state() {
    for point in PUBLISH_POINTS {
        let faults = Arc::new(ScriptedFaults::new().fail_once(point));
        let fixture = Fixture::with_faults(&format!("publish-error-{point:?}"), faults.clone());

        let error = fixture
            .try_publish(ArtifactContent::SingleHtml(OLD.to_owned()))
            .await
            .expect_err("the armed fault aborts the publish");
        assert!(matches!(error, AppError::Unavailable(_)), "{point:?}");
        assert!(faults.all_fired(), "{point:?} was never reached");

        assert_eq!(
            fixture.count("SELECT COUNT(*) FROM artifacts"),
            0,
            "{point:?}: the inserted row is compensated away"
        );
        assert!(
            fixture.entries().is_empty(),
            "{point:?}: staging and final paths are both removed, found {:?}",
            fixture.entries()
        );
        assert_audit_converges(&fixture, &format!("publish-error-{point:?}")).await;
    }
}

#[tokio::test]
async fn publish_compensation_delete_failure_preserves_the_recoverable_body() {
    let faults = Arc::new(
        ScriptedFaults::new()
            .fail_once(FaultPoint::PublishRename)
            .fail_once(FaultPoint::PublishDeleteRow),
    );
    let fixture = Fixture::with_faults("publish-error-delete-row", faults.clone());

    let error = fixture
        .try_publish(ArtifactContent::SingleHtml(OLD.to_owned()))
        .await
        .expect_err("the rename and compensation delete both fail");
    assert!(faults.all_fired(), "both faults must be reached");

    assert_eq!(
        fixture.count("SELECT COUNT(*) FROM artifacts"),
        1,
        "the row deletion failed"
    );
    assert!(
        fixture
            .entries()
            .iter()
            .all(|name| name.contains(".staging-")),
        "the final rename never happened"
    );
    let staging = fixture.staging_entries();
    assert_eq!(
        staging.len(),
        1,
        "staging is the only recoverable body and must not be removed"
    );
    assert_eq!(
        std::fs::read_to_string(fixture.artifact_dir.join(&staging[0]))
            .expect("read recoverable staging body"),
        OLD
    );
    assert_eq!(
        error,
        AppError::Unavailable("injected fault at PublishDeleteRow".to_owned()),
        "Node propagates the failed compensating delete"
    );
}

#[tokio::test]
async fn publish_crashes_are_repaired_by_startup_reconciliation() {
    // (failpoint, does the artifact survive the crash?)
    let cases = [
        (FaultPoint::PublishStageWrite, false),
        (FaultPoint::PublishStageFileSync, false),
        (FaultPoint::PublishStageDirectorySync, false),
        (FaultPoint::PublishInsert, false),
        (FaultPoint::PublishRename, true),
        (FaultPoint::PublishComplete, true),
    ];
    for (point, survives) in cases {
        let context = format!("publish-crash-{point:?}");
        let faults = Arc::new(ScriptedFaults::new().crash_once(point));
        let fixture = Fixture::with_faults(&context, faults.clone());
        fixture
            .try_publish(ArtifactContent::SingleHtml(OLD.to_owned()))
            .await
            .expect_err("the armed crash aborts the publish");
        assert!(
            faults.all_fired(),
            "{context}: the failpoint was never reached"
        );

        let report = fixture
            .store
            .audit_storage(true)
            .await
            .expect("startup audit runs");
        assert!(
            report.orphan_bodies.is_empty() && report.divergent_bodies.is_empty(),
            "{context}: unexpected divergence {report:?}"
        );

        match sole_artifact(&fixture).await {
            Some(meta) => {
                assert!(survives, "{context}: the row should not have survived");
                let body = fixture
                    .body_on_disk(&meta)
                    .unwrap_or_else(|| panic!("{context}: the committed body must be recovered"));
                assert_eq!(body, OLD, "{context}");
                assert_eq!(sha256_hex(&body), meta.body_sha256, "{context}");
                assert!(report.missing_bodies.is_empty(), "{context}: {report:?}");
            }
            None => {
                assert!(!survives, "{context}: the row should have survived");
                assert!(
                    fixture.entries().is_empty(),
                    "{context}: an unreferenced staged body is discarded, found {:?}",
                    fixture.entries()
                );
            }
        }
        assert!(
            fixture.transient_entries().is_empty(),
            "{context}: reconciliation clears every transient path"
        );
        assert_audit_converges(&fixture, &context).await;
    }
}

#[tokio::test]
async fn post_rename_directory_barriers_leave_each_lifecycle_operation_recoverable() {
    // These faults are deliberately *after* the kernel rename and *before* the parent directory
    // fsync. They model the physical partial state a process can return from: the namespace has
    // changed, but the caller has no durable acknowledgement. The intent must remain concealed
    // until startup proves the right recovery action.

    let publish_faults =
        Arc::new(ScriptedFaults::new().fail_once(FaultPoint::PublishRenameBarrier));
    let publish = Fixture::with_faults("publish-rename-directory-sync", publish_faults.clone());
    publish
        .try_publish(ArtifactContent::SingleHtml(OLD.to_owned()))
        .await
        .expect_err("the post-rename directory barrier fails");
    assert!(publish_faults.all_fired());
    assert!(
        sole_artifact(&publish).await.is_none(),
        "prepared publish is concealed"
    );
    publish
        .store
        .audit_storage(true)
        .await
        .expect("startup proves and completes the publish");
    let published = sole_artifact(&publish).await.expect("publish recovered");
    assert_eq!(publish.body_on_disk(&published).as_deref(), Some(OLD));
    assert_audit_converges(&publish, "publish-rename-directory-sync").await;

    let update_faults = Arc::new(ScriptedFaults::new().fail_once(FaultPoint::UpdateSwapBarrier));
    let update = Fixture::with_faults("update-rename-directory-sync", update_faults.clone());
    let prior = update.publish_single(OLD).await;
    update
        .store
        .update_for(&prior, html_update(1, NEW), mutation_audit())
        .await
        .expect_err("the post-rename directory barrier fails");
    assert!(update_faults.all_fired());
    assert!(
        sole_artifact(&update).await.is_none(),
        "prepared update is concealed"
    );
    update
        .store
        .audit_storage(true)
        .await
        .expect("startup proves and completes the update");
    let updated = sole_artifact(&update).await.expect("update recovered");
    assert_eq!(updated.revision, 2);
    assert_eq!(update.body_on_disk(&updated).as_deref(), Some(NEW));
    assert_eq!(update.history_body(&updated, 1).as_deref(), Some(OLD));
    assert_audit_converges(&update, "update-rename-directory-sync").await;

    let snapshot_faults =
        Arc::new(ScriptedFaults::new().fail_once(FaultPoint::UpdateSnapshotBarrier));
    let snapshot = Fixture::with_faults("update-snapshot-directory-sync", snapshot_faults.clone());
    let snapshot_prior = snapshot.publish_single(OLD).await;
    snapshot
        .store
        .update_for(&snapshot_prior, html_update(1, NEW), mutation_audit())
        .await
        .expect_err("the history-rename directory barrier fails");
    assert!(snapshot_faults.all_fired());
    assert!(
        sole_artifact(&snapshot).await.is_none(),
        "prepared update is concealed"
    );
    snapshot
        .store
        .audit_storage(true)
        .await
        .expect("startup installs the committed staged replacement");
    let snapshot_updated = sole_artifact(&snapshot)
        .await
        .expect("snapshot-barrier update recovered");
    assert_eq!(
        snapshot.body_on_disk(&snapshot_updated).as_deref(),
        Some(NEW)
    );
    assert_eq!(
        snapshot.history_body(&snapshot_updated, 1).as_deref(),
        Some(OLD)
    );
    assert_audit_converges(&snapshot, "update-snapshot-directory-sync").await;

    let delete_faults =
        Arc::new(ScriptedFaults::new().fail_once(FaultPoint::DeleteTrashRenameBarrier));
    let delete = Fixture::with_faults("delete-rename-directory-sync", delete_faults.clone());
    let doomed = delete.publish_single(OLD).await;
    delete
        .store
        .delete_for(&doomed, mutation_audit())
        .await
        .expect_err("the post-rename directory barrier fails");
    assert!(delete_faults.all_fired());
    assert!(
        sole_artifact(&delete).await.is_none(),
        "prepared delete is concealed"
    );
    delete
        .store
        .audit_storage(true)
        .await
        .expect("startup restores an interrupted delete");
    let restored = sole_artifact(&delete)
        .await
        .expect("delete rollback recovered");
    assert_eq!(delete.body_on_disk(&restored).as_deref(), Some(OLD));
    assert_audit_converges(&delete, "delete-rename-directory-sync").await;
}

#[tokio::test]
async fn history_directory_creation_barrier_fails_closed_and_a_retry_can_snapshot() {
    let faults = Arc::new(ScriptedFaults::new().fail_once(FaultPoint::UpdateHistoryDirectorySync));
    let fixture = Fixture::with_faults("history-directory-create-barrier", faults.clone());
    let meta = fixture.publish_single(OLD).await;
    let error = fixture
        .store
        .update_for(&meta, html_update(1, NEW), mutation_audit())
        .await
        .expect_err("the first newly-created history-directory barrier fails");
    assert!(matches!(error, AppError::Unavailable(_)));
    assert!(faults.all_fired());
    let unchanged = fixture
        .reload(&meta)
        .expect("compensation restores metadata");
    assert_eq!(unchanged.revision, 1);
    assert_eq!(fixture.body_on_disk(&unchanged).as_deref(), Some(OLD));
    assert_eq!(
        fixture.count("SELECT COUNT(*) FROM artifact_durability_intents"),
        0,
        "the failed directory creation did not acknowledge a concealed revision"
    );

    let retried = fixture
        .store
        .update_for(&unchanged, html_update(1, NEW), mutation_audit())
        .await
        .expect("a later update can create and durably snapshot the same history path");
    assert_eq!(retried.meta.revision, 2);
    assert_eq!(fixture.history_body(&retried.meta, 1).as_deref(), Some(OLD));
    assert_audit_converges(&fixture, "history-directory-create-barrier").await;
}

#[tokio::test]
async fn metadata_snapshot_rename_barrier_keeps_a_recoverable_intent_not_a_retry_dead_end() {
    let faults = Arc::new(ScriptedFaults::new().fail_once(FaultPoint::UpdateSnapshotBarrier));
    let fixture = Fixture::with_faults("metadata-snapshot-rename-barrier", faults.clone());
    let meta = fixture.publish_single(OLD).await;
    fixture
        .store
        .update_for(
            &meta,
            ArtifactUpdate {
                expected_revision: 1,
                title: Some("Retitled".to_owned()),
                ..ArtifactUpdate::default()
            },
            mutation_audit(),
        )
        .await
        .expect_err("the history rename completed before its directory barrier failed");
    assert!(faults.all_fired());
    assert!(
        sole_artifact(&fixture).await.is_none(),
        "intent conceals the partial revision"
    );
    assert_eq!(fixture.history_body(&meta, 1).as_deref(), Some(OLD));

    fixture
        .store
        .audit_storage(true)
        .await
        .expect("recovery validates the immutable snapshot and releases the intent");
    let recovered = sole_artifact(&fixture)
        .await
        .expect("metadata revision is readable");
    assert_eq!(recovered.revision, 2);
    let retried = fixture
        .store
        .update_for(
            &recovered,
            ArtifactUpdate {
                expected_revision: 2,
                title: Some("Retried".to_owned()),
                ..ArtifactUpdate::default()
            },
            mutation_audit(),
        )
        .await
        .expect("a fresh history destination is available after recovery");
    assert_eq!(retried.meta.revision, 3);
    assert_audit_converges(&fixture, "metadata-snapshot-rename-barrier").await;
}

// ---------------------------------------------------------------------------
// update
// ---------------------------------------------------------------------------

/// Publish `OLD`, arm `point`, then try to replace the body with `NEW`.
async fn staged_update(context: &str, faults: Arc<ScriptedFaults>) -> (Fixture, ArtifactMeta) {
    let fixture = Fixture::with_faults(context, faults);
    let meta = fixture.publish_single(OLD).await;
    (fixture, meta)
}

#[tokio::test]
async fn update_compensation_restores_the_previous_revision_exactly() {
    // Every boundary before the swap completes must leave revision 1 with the OLD body.
    let points = [
        FaultPoint::UpdateStageWrite,
        FaultPoint::UpdateStageFileSync,
        FaultPoint::UpdateStageDirectorySync,
        FaultPoint::UpdateCommit,
        FaultPoint::UpdateCommitTransaction,
        FaultPoint::UpdateSnapshot,
        FaultPoint::UpdateSwap,
    ];
    for point in points {
        let context = format!("update-error-{point:?}");
        let faults = Arc::new(ScriptedFaults::new().fail_once(point));
        let (fixture, meta) = staged_update(&context, faults.clone()).await;

        let error = fixture
            .store
            .update_for(&meta, html_update(1, NEW), mutation_audit())
            .await
            .expect_err("the armed fault aborts the update");
        assert!(matches!(error, AppError::Unavailable(_)), "{context}");
        assert!(
            faults.all_fired(),
            "{context}: the failpoint was never reached"
        );

        let row = fixture.reload(&meta).unwrap_or_else(|| panic!("{context}"));
        assert_eq!(row.revision, 1, "{context}: the revision is reverted");
        assert_eq!(row.body_sha256, sha256_hex(OLD), "{context}");
        assert_eq!(row.updated_at, meta.updated_at, "{context}");
        assert_eq!(
            fixture.body_on_disk(&meta).as_deref(),
            Some(OLD),
            "{context}: the prior body is still installed"
        );
        assert_eq!(
            fixture.count("SELECT COUNT(*) FROM artifact_revisions"),
            1,
            "{context}: only the live attribution marker remains"
        );
        assert!(
            fixture.history_entries(&meta).is_empty(),
            "{context}: the snapshot is moved back out of history"
        );

        if point == FaultPoint::UpdateCommitTransaction {
            // The metadata transaction throwing is the one path Node does not wrap in a `catch`
            // ([lib/store.js:406-415] sits outside the try), so the staged body is left for
            // startup reconciliation. It is unreferenced — the installed body already matches the
            // committed digest — so it is discarded, not installed.
            assert_eq!(
                fixture.staging_entries().len(),
                1,
                "{context}: the staged body survives for reconciliation"
            );
            let report = fixture
                .store
                .audit_storage(true)
                .await
                .expect("startup audit runs");
            assert!(
                report.recovered_paths.is_empty(),
                "{context}: an uncommitted staged body is never installed: {report:?}"
            );
            assert_eq!(
                fixture.body_on_disk(&meta).as_deref(),
                Some(OLD),
                "{context}"
            );
        }
        assert!(
            fixture.transient_entries().is_empty(),
            "{context}: no staged body outlives reconciliation, found {:?}",
            fixture.entries()
        );
        assert_audit_converges(&fixture, &context).await;
    }
}

#[tokio::test]
async fn update_refuses_missing_or_divergent_predecessors_before_committing_a_revision() {
    for (label, corrupt, expected) in [
        ("missing", None, AppError::Gone("body_missing".to_owned())),
        (
            "divergent",
            Some("<p>tampered</p>"),
            AppError::Conflict("body_digest_mismatch".to_owned()),
        ),
    ] {
        let fixture = Fixture::new(&format!("update-predecessor-{label}"));
        let meta = fixture.publish_single(OLD).await;
        let body = fixture.artifact_dir.join(format!("{}.html", meta.id.0));
        match corrupt {
            Some(contents) => std::fs::write(&body, contents).expect("tamper body"),
            None => std::fs::remove_file(&body).expect("remove body"),
        }
        assert_eq!(
            fixture
                .store
                .update_for(
                    &meta,
                    ArtifactUpdate {
                        expected_revision: 1,
                        title: Some("must not commit".to_owned()),
                        ..ArtifactUpdate::default()
                    },
                    mutation_audit()
                )
                .await,
            Err(expected),
            "{label} predecessor is rejected before a history row or intent is admitted"
        );
        assert_eq!(fixture.reload(&meta).expect("row remains").revision, 1);
        assert_eq!(fixture.count("SELECT COUNT(*) FROM artifact_revisions"), 1);
        assert_eq!(
            fixture.count("SELECT COUNT(*) FROM artifact_durability_intents"),
            0
        );
        assert!(fixture.history_entries(&meta).is_empty());
    }
}

#[tokio::test]
async fn a_fault_after_the_swap_leaves_a_fully_committed_update() {
    // History pruning is best effort; failing there must not undo a committed revision.
    let context = "update-error-prune";
    let faults = Arc::new(ScriptedFaults::new().fail_once(FaultPoint::UpdatePrune));
    let (fixture, meta) = staged_update(context, faults.clone()).await;

    fixture
        .store
        .update_for(&meta, html_update(1, NEW), mutation_audit())
        .await
        .expect_err("the armed fault aborts after the swap");
    assert!(faults.all_fired());

    let row = fixture.reload(&meta).expect("row survives");
    assert_eq!(row.revision, 2);
    assert_eq!(row.body_sha256, sha256_hex(NEW));
    assert_eq!(fixture.body_on_disk(&meta).as_deref(), Some(NEW));
    assert_body_matches_metadata(&fixture, &row, context);
    assert_audit_converges(&fixture, context).await;
}

#[tokio::test]
async fn update_crashes_never_lose_the_prior_or_the_committed_body() {
    // (failpoint, is the new revision committed at the moment of the crash?)
    let cases = [
        (FaultPoint::UpdateStageWrite, false),
        (FaultPoint::UpdateStageFileSync, false),
        (FaultPoint::UpdateStageDirectorySync, false),
        (FaultPoint::UpdateCommit, false),
        (FaultPoint::UpdateCommitTransaction, false),
        (FaultPoint::UpdateSnapshot, true),
        (FaultPoint::UpdateSwap, true),
        (FaultPoint::UpdatePrune, true),
    ];
    for (point, committed) in cases {
        let context = format!("update-crash-{point:?}");
        let faults = Arc::new(ScriptedFaults::new().crash_once(point));
        let (fixture, meta) = staged_update(&context, faults.clone()).await;

        fixture
            .store
            .update_for(&meta, html_update(1, NEW), mutation_audit())
            .await
            .expect_err("the armed crash aborts the update");
        assert!(
            faults.all_fired(),
            "{context}: the failpoint was never reached"
        );

        let report = fixture
            .store
            .audit_storage(true)
            .await
            .expect("startup audit runs");
        let row = fixture.reload(&meta).unwrap_or_else(|| panic!("{context}"));

        if committed {
            assert_eq!(row.revision, 2, "{context}");
            assert_eq!(row.body_sha256, sha256_hex(NEW), "{context}");
            assert_eq!(
                fixture.body_on_disk(&meta).as_deref(),
                Some(NEW),
                "{context}: the committed replacement is installed by recovery"
            );
        } else {
            assert_eq!(row.revision, 1, "{context}");
            assert_eq!(
                fixture.body_on_disk(&meta).as_deref(),
                Some(OLD),
                "{context}: an uncommitted body is never served"
            );
        }
        assert_body_matches_metadata(&fixture, &row, &context);
        assert!(
            report.divergent_bodies.is_empty() && report.missing_bodies.is_empty(),
            "{context}: recovery left divergence {report:?}"
        );
        assert!(
            fixture.transient_entries().is_empty(),
            "{context}: reconciliation clears every transient path"
        );
        assert_audit_converges(&fixture, &context).await;

        if point == FaultPoint::UpdateSnapshot {
            assert_eq!(
                fixture.history_body(&meta, 1).as_deref(),
                Some(OLD),
                "{context}: recovery preserves revision 1 before installing revision 2"
            );
            let restored = fixture
                .store
                .restore_for(&row, 1, None, mutation_audit())
                .await
                .expect("revision 1 remains restorable after recovery");
            assert_eq!(restored.restored_from, 1, "{context}");
            assert_eq!(
                fixture.body_on_disk(&meta).as_deref(),
                Some(OLD),
                "{context}: restoring revision 1 reinstalls its body"
            );
        }
    }
}

#[tokio::test]
async fn the_commit_then_swap_window_preserves_history_and_reinstalls_the_committed_body() {
    // The ADR-0002 window, spelled out: crash after the metadata commit and before the swap.
    // The staged file IS the committed content, and reconciliation installs it by digest.
    let context = "update-crash-window";
    let faults = Arc::new(ScriptedFaults::new().crash_once(FaultPoint::UpdateSnapshot));
    let (fixture, meta) = staged_update(context, faults).await;
    fixture
        .store
        .update_for(&meta, html_update(1, NEW), mutation_audit())
        .await
        .expect_err("crash before the snapshot");

    // Pre-audit: metadata says NEW, the disk still holds OLD, and the staged body survives.
    let crashed = fixture.reload(&meta).expect("row");
    assert_eq!(crashed.revision, 2);
    assert_eq!(crashed.body_sha256, sha256_hex(NEW));
    assert_eq!(fixture.body_on_disk(&meta).as_deref(), Some(OLD));
    assert_eq!(fixture.staging_entries().len(), 1);

    let observed = fixture
        .store
        .audit_storage(false)
        .await
        .expect("a read-only audit reports without repairing");
    assert_eq!(observed.divergent_bodies, vec![meta.id.0.clone()]);
    assert_eq!(observed.transient_paths.len(), 1);
    assert!(observed.recovered_paths.is_empty());
    assert_eq!(
        fixture.body_on_disk(&meta).as_deref(),
        Some(OLD),
        "a read-only audit must not touch anything"
    );

    let repaired = fixture
        .store
        .audit_storage(true)
        .await
        .expect("startup audit repairs");
    assert_eq!(repaired.recovered_paths.len(), 1);
    assert!(repaired.divergent_bodies.is_empty());
    assert_eq!(fixture.body_on_disk(&meta).as_deref(), Some(NEW));

    let current = fixture.reload(&meta).expect("repaired row");
    assert_eq!(
        fixture.history_body(&meta, 1).as_deref(),
        Some(OLD),
        "recovery snapshots revision 1 before installing revision 2"
    );
    let restored = fixture
        .store
        .restore_for(&current, 1, None, mutation_audit())
        .await
        .expect("revision 1 remains restorable");
    assert_eq!(restored.restored_from, 1);
    assert_eq!(restored.meta.revision, 3);
    assert_eq!(fixture.body_on_disk(&meta).as_deref(), Some(OLD));
}

#[tokio::test]
async fn metadata_only_revision_crash_recovers_its_history_before_becoming_readable() {
    // A title/category-only update has no replacement staging body, but its revision still owns
    // an immutable copy of the current body. Crash before that copy and startup must reconstruct
    // it before clearing the lifecycle marker.
    let faults = Arc::new(ScriptedFaults::new().crash_once(FaultPoint::UpdateSnapshot));
    let fixture = Fixture::with_faults("metadata-only-history-crash", faults.clone());
    let meta = fixture.publish_single(OLD).await;
    fixture
        .store
        .update_for(
            &meta,
            ArtifactUpdate {
                expected_revision: 1,
                title: Some("Retitled".to_owned()),
                ..ArtifactUpdate::default()
            },
            mutation_audit(),
        )
        .await
        .expect_err("crash before the metadata-only history copy");
    assert!(faults.all_fired());
    assert!(
        sole_artifact(&fixture).await.is_none(),
        "incomplete revision is concealed"
    );
    assert!(
        fixture.history_entries(&meta).is_empty(),
        "no snapshot has reached history yet"
    );

    let observed = fixture
        .store
        .audit_storage(false)
        .await
        .expect("read-only audit reports only");
    assert!(observed.recovered_paths.is_empty());
    assert!(
        fixture.history_entries(&meta).is_empty(),
        "read-only audit never copies history"
    );

    fixture
        .store
        .audit_storage(true)
        .await
        .expect("startup reconstructs metadata-only history");
    let current = sole_artifact(&fixture)
        .await
        .expect("revision becomes ready after recovery");
    assert_eq!(current.revision, 2);
    assert_eq!(current.title, "Retitled");
    assert_eq!(fixture.body_on_disk(&current).as_deref(), Some(OLD));
    assert_eq!(fixture.history_body(&current, 1).as_deref(), Some(OLD));
    assert_audit_converges(&fixture, "metadata-only-history-crash").await;
}

#[tokio::test]
async fn metadata_only_snapshot_sync_and_cleanup_failures_recover_without_blocking_retry() {
    for (is_bundle, cleanup_point, temp_survives) in [
        (false, FaultPoint::UpdateSnapshotTempRemove, true),
        (true, FaultPoint::UpdateSnapshotTempRemove, true),
        (false, FaultPoint::UpdateSnapshotTempRemoveBarrier, false),
        (true, FaultPoint::UpdateSnapshotTempRemoveBarrier, false),
    ] {
        let kind = if is_bundle { "bundle" } else { "single" };
        let cleanup = if temp_survives { "unlink" } else { "barrier" };
        let faults = Arc::new(
            ScriptedFaults::new()
                .fail_once(FaultPoint::UpdateSnapshotFileSync)
                .fail_once(cleanup_point),
        );
        let fixture = Fixture::with_faults(
            &format!("metadata-snapshot-temp-{kind}-{cleanup}"),
            faults.clone(),
        );
        let meta = if is_bundle {
            fixture
                .publish_bundle(
                    &[("index.html", "<h1>stable</h1>"), ("app.js", "stable();")],
                    Some("index.html"),
                )
                .await
                .meta
        } else {
            fixture.publish_single(OLD).await
        };
        let snapshot_temporary = fixture
            .artifact_dir
            .join(".history")
            .join(&meta.id.0)
            .join("1.snapshot-tmp");

        let error = fixture
            .store
            .update_for(
                &meta,
                ArtifactUpdate {
                    expected_revision: 1,
                    title: Some("Interrupted title".to_owned()),
                    ..ArtifactUpdate::default()
                },
                mutation_audit(),
            )
            .await
            .expect_err("snapshot sync and compensating cleanup are both interrupted");
        assert!(matches!(error, AppError::Unavailable(_)), "{kind}");
        assert!(faults.all_fired(), "{kind}: both failpoints must fire");
        assert_eq!(
            fixture.reload(&meta).expect("reverted row").revision,
            1,
            "{kind}: metadata compensation restores the prior revision"
        );
        assert!(
            sole_artifact(&fixture).await.is_none(),
            "{kind}: the retained durability intent conceals the reverted row"
        );
        assert!(
            snapshot_temporary.exists() == temp_survives,
            "{kind}/{cleanup}: the temp's presence reflects the exact cleanup boundary"
        );
        assert_eq!(
            fixture.count("SELECT COUNT(*) FROM artifact_durability_intents"),
            1,
            "{kind}/{cleanup}: cleanup failure must retain the recovery marker"
        );

        fixture
            .store
            .audit_storage(true)
            .await
            .expect("startup audit removes the reverted update's owned temporary");
        assert!(
            !snapshot_temporary.exists(),
            "{kind}/{cleanup}: audit durably confirms removal of the exact snapshot temporary"
        );
        assert_eq!(
            fixture.count("SELECT COUNT(*) FROM artifact_durability_intents"),
            0,
            "{kind}/{cleanup}: the marker clears only after cleanup succeeds"
        );
        assert_eq!(
            fixture.count("SELECT COUNT(*) FROM security_audit_receipts WHERE state = 'pending'"),
            0,
            "{kind}/{cleanup}: reconciliation must terminalize the old receipt before retry"
        );
        assert_eq!(
            fixture.count(
                "SELECT COUNT(*) FROM security_audit_receipts WHERE durability_intent_id LIKE 'update:%'",
            ),
            0,
            "{kind}/{cleanup}: a proven compensation must release its deterministic receipt"
        );
        let recovered = sole_artifact(&fixture)
            .await
            .expect("the reverted artifact becomes visible");
        assert_eq!(recovered.revision, 1, "{kind}/{cleanup}");

        let retried = fixture
            .store
            .update_for(
                &recovered,
                ArtifactUpdate {
                    expected_revision: 1,
                    title: Some("Retried title".to_owned()),
                    ..ArtifactUpdate::default()
                },
                mutation_audit(),
            )
            .await
            .expect("the next metadata-only update is no longer blocked");
        assert_eq!(retried.meta.revision, 2, "{kind}/{cleanup}");
        assert!(
            fixture
                .history_entries(&retried.meta)
                .iter()
                .all(|name| !name.ends_with(".snapshot-tmp")),
            "{kind}/{cleanup}: no orphan snapshot temporary remains"
        );
        assert_eq!(
            fixture.count("SELECT COUNT(*) FROM artifact_durability_intents"),
            0,
            "{kind}/{cleanup}: the successful retry leaves no concealment marker"
        );
    }
}

#[tokio::test]
async fn prepared_marker_with_an_already_committed_metadata_only_revision_recovers_history() {
    // Compatibility proof for an older process that died after committing revision 2 but before
    // advancing its intent. New writes advance both in one transaction; recovery still infers
    // this durable target revision instead of clearing `prepared` without history.
    let fixture = Fixture::new("prepared-metadata-commit-recovery");
    let meta = fixture.publish_single(OLD).await;
    fixture.execute(&format!(
        "UPDATE artifacts SET title = 'Retitled', revision = 2 WHERE id = '{}';\
         INSERT INTO artifact_durability_intents \
           (id, artifact_id, operation, state, expected_sha256, prior_sha256, staging_path) \
         VALUES ('update:{}:2', '{}', 'update', 'prepared', '{}', '{}', '.{}.staging-prepared');",
        meta.id.0, meta.id.0, meta.id.0, meta.body_sha256, meta.body_sha256, meta.id.0
    ));
    assert!(sole_artifact(&fixture).await.is_none());

    fixture
        .store
        .audit_storage(true)
        .await
        .expect("recovery infers the committed target revision");
    let recovered = sole_artifact(&fixture)
        .await
        .expect("history-backed revision is readable");
    assert_eq!(recovered.revision, 2);
    assert_eq!(fixture.history_body(&recovered, 1).as_deref(), Some(OLD));
    assert_eq!(
        fixture.count("SELECT COUNT(*) FROM artifact_durability_intents"),
        0
    );
}

#[tokio::test]
async fn compensated_metadata_only_marker_clears_only_for_an_unambiguous_next_revision_target() {
    for (case, target, clears) in [("reverted", "2", true), ("malformed", "later", false)] {
        // Model compensation that restored the artifact row to revision 1 but failed before it
        // could delete the already-advanced intent. The live prior body is intact.
        let fixture = Fixture::new(&format!("metadata-marker-{case}"));
        let meta = fixture.publish_single(OLD).await;
        fixture.execute(&format!(
            "INSERT INTO artifact_durability_intents \
               (id, artifact_id, operation, state, expected_sha256, prior_sha256, staging_path) \
             VALUES ('update:{}:{}', '{}', 'update', 'metadata_committed', '{}', '{}', '.{}.staging-rollback');",
            meta.id.0, target, meta.id.0, meta.body_sha256, meta.body_sha256, meta.id.0
        ));
        assert!(
            sole_artifact(&fixture).await.is_none(),
            "marker initially conceals {case}"
        );

        fixture
            .store
            .audit_storage(true)
            .await
            .expect("clean audit classifies the rollback marker");
        assert_eq!(
            fixture.count("SELECT COUNT(*) FROM artifact_durability_intents"),
            i64::from(!clears),
            "{case}: only a well-formed next-revision marker is proven reverted"
        );
        assert_eq!(
            sole_artifact(&fixture).await.is_some(),
            clears,
            "{case}: visibility follows the conservative marker decision"
        );
        assert!(
            fixture.history_entries(&meta).is_empty(),
            "a reverted revision never manufactures revision-zero history"
        );
    }
}

#[tokio::test]
async fn both_recovery_passes_retain_metadata_only_intents_without_a_valid_outgoing_history() {
    for case in ["corrupt-history", "missing-outgoing-revision"] {
        let fixture = Fixture::new(&format!("metadata-history-validator-{case}"));
        let meta = fixture.publish_single(OLD).await;
        fixture.execute(&format!(
            "UPDATE artifacts SET title = 'Retitled', revision = 2 WHERE id = '{}';\
             INSERT INTO artifact_durability_intents \
               (id, artifact_id, operation, state, expected_sha256, prior_sha256, staging_path) \
             VALUES ('update:{}:2', '{}', 'update', 'metadata_committed', '{}', '{}', '.{}.staging-validator');",
            meta.id.0, meta.id.0, meta.id.0, meta.body_sha256, meta.body_sha256, meta.id.0
        ));
        if case == "corrupt-history" {
            let history = fixture
                .artifact_dir
                .join(".history")
                .join(&meta.id.0)
                .join("1.html");
            std::fs::create_dir_all(history.parent().expect("history parent"))
                .expect("create corrupt history parent");
            std::fs::write(history, "<p>corrupt</p>").expect("write corrupt history");
        } else {
            fixture.execute(&format!(
                "DELETE FROM artifact_revisions WHERE artifact_id = '{}' AND revision = 1;",
                meta.id.0
            ));
        }

        fixture
            .store
            .audit_storage(true)
            .await
            .expect("ambiguous history is retained for retry/operator repair");
        assert!(
            sole_artifact(&fixture).await.is_none(),
            "{case} remains concealed"
        );
        assert_eq!(
            fixture.count("SELECT COUNT(*) FROM artifact_durability_intents"),
            1,
            "{case}: the second recovery pass must not clear the retained marker"
        );
    }
}

#[tokio::test]
async fn restore_linearizes_its_history_read_and_replay_against_a_concurrent_update() {
    // Revision 1 is retained when revision 2 replaces its body. Pause restore at the first
    // mutating point of its replay, after it has read revision 1. A stale update must remain
    // outside the lifecycle write gate until restore completes, then conflict on revision 3.
    let blocker = Arc::new(BlockingUpdateFault::new());
    let fixture = Fixture::with_custom_injector("restore-write-gate", blocker.clone());
    let initial = fixture.publish_single(OLD).await;
    let current = fixture
        .store
        .update_for(&initial, html_update(1, NEW), mutation_audit())
        .await
        .expect("create restorable revision one")
        .meta;

    blocker.arm();
    let restoring_store = fixture.store.clone();
    let restoring_meta = current.clone();
    let restore = tokio::spawn(async move {
        restoring_store
            .restore_for(&restoring_meta, 1, None, mutation_audit())
            .await
    });
    let entered = blocker.entered.clone();
    tokio::task::spawn_blocking(move || entered.wait())
        .await
        .expect("observe restore at its replay boundary");

    let updating_store = fixture.store.clone();
    let stale_meta = current.clone();
    let update = tokio::spawn(async move {
        updating_store
            .update_for(
                &stale_meta,
                ArtifactUpdate {
                    expected_revision: 2,
                    title: Some("must not interleave".to_owned()),
                    ..ArtifactUpdate::default()
                },
                mutation_audit(),
            )
            .await
    });
    let mut update = update;
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(150), &mut update)
            .await
            .is_err(),
        "the stale mutation must wait while restore holds the lifecycle gate"
    );

    let release = blocker.release.clone();
    tokio::task::spawn_blocking(move || release.wait())
        .await
        .expect("release restore");
    let restored = restore
        .await
        .expect("restore task joins")
        .expect("restore succeeds");
    assert_eq!(restored.meta.revision, 3);
    assert_eq!(
        update.await.expect("stale update task joins"),
        Err(AppError::Conflict("conflict".to_owned())),
        "the update sees the restore's revision instead of racing its source read"
    );
}

#[tokio::test]
async fn thumbnail_port_holds_the_current_digest_read_against_update_and_delete() {
    // HTTP and MCP thumbnail handlers both delegate to this ArtifactService port. Hold the
    // preview adapter in its synchronous file read and prove neither mutation can replace or
    // remove the body between the production current-digest check and that read.
    let fixture = Fixture::new("thumbnail-read-gate");
    let meta = fixture.publish_single(OLD).await;
    let authorized =
        AccessPolicy::authorize_publisher_read(&publisher(), Some(meta.clone()), &meta.id.0)
            .expect("publisher may read its artifact")
            .into_authorized();
    let previews = Arc::new(BlockingPreview::new());

    for mutation in ["update", "delete"] {
        previews.arm();
        let reader_store = fixture.store.clone();
        let reader_artifact = authorized.clone();
        let reader_previews: Arc<dyn PreviewService> = previews.clone();
        let digest = meta.body_sha256.clone();
        let reader = tokio::spawn(async move {
            reader_store
                .read_current_thumbnail(&reader_artifact, &digest, reader_previews)
                .await
        });
        let entered = previews.entered.clone();
        tokio::task::spawn_blocking(move || entered.wait())
            .await
            .expect("observe thumbnail file read under guard");

        let mutator_store = fixture.store.clone();
        let mutator_meta = fixture.reload(&meta).expect("artifact still exists");
        let mutator = if mutation == "update" {
            tokio::spawn(async move {
                mutator_store
                    .update_for(
                        &mutator_meta,
                        ArtifactUpdate {
                            expected_revision: mutator_meta.revision,
                            title: Some("changed after preview".to_owned()),
                            ..ArtifactUpdate::default()
                        },
                        mutation_audit(),
                    )
                    .await
                    .map(|_| ())
            })
        } else {
            tokio::spawn(async move {
                mutator_store
                    .delete_for(&mutator_meta, mutation_audit())
                    .await
                    .map(|_| ())
            })
        };
        let mut mutator = mutator;
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(150), &mut mutator)
                .await
                .is_err(),
            "{mutation} must wait for the thumbnail port's linearized read"
        );

        let release = previews.release.clone();
        tokio::task::spawn_blocking(move || release.wait())
            .await
            .expect("release thumbnail read");
        assert_eq!(
            reader
                .await
                .expect("thumbnail task joins")
                .expect("thumbnail read succeeds"),
            Some(b"preview bytes".to_vec())
        );
        mutator
            .await
            .expect("mutation task joins")
            .expect("mutation proceeds after preview read");

        if mutation == "update" {
            // The same still-current artifact is used for the delete half of this proof.
            continue;
        }
    }
}

#[tokio::test]
async fn thumbnail_port_rechecks_tenant_identity_after_a_retenant() {
    let fixture = Fixture::new("thumbnail-retentant-identity");
    fixture.create_org("other");
    let meta = fixture.publish_single(OLD).await;
    let old_tenant_grant =
        AccessPolicy::authorize_publisher_read(&publisher(), Some(meta.clone()), &meta.id.0)
            .expect("old tenant grant")
            .into_authorized();
    fixture
        .store
        .move_to_org_for(&meta, "other", None, mutation_audit())
        .await
        .expect("re-tenant before thumbnail read acquires its guard");

    let previews: Arc<dyn PreviewService> = Arc::new(BlockingPreview::new());
    assert_eq!(
        fixture
            .store
            .read_current_thumbnail(&old_tenant_grant, &meta.body_sha256, previews)
            .await
            .expect("the stale authorization is safely rechecked"),
        None,
        "an old-tenant authorization cannot read an unchanged-digest thumbnail"
    );
}

#[tokio::test]
async fn a_failed_compensation_still_preserves_both_bodies() {
    // Double fault: the swap fails AND moving the snapshot back fails. The committed metadata
    // stays, the prior body sits in history, and the staged body is still recoverable.
    let context = "update-error-restore-snapshot";
    let faults = Arc::new(
        ScriptedFaults::new()
            .fail_once(FaultPoint::UpdateSwap)
            .fail_once(FaultPoint::UpdateRestoreSnapshot),
    );
    let (fixture, meta) = staged_update(context, faults.clone()).await;
    fixture
        .store
        .update_for(&meta, html_update(1, NEW), mutation_audit())
        .await
        .expect_err("both the swap and its compensation fail");
    assert!(faults.all_fired());

    assert_eq!(
        fixture.history_body(&meta, 1).as_deref(),
        Some(OLD),
        "the prior body is still retained in history"
    );
    let report = fixture
        .store
        .audit_storage(true)
        .await
        .expect("startup audit repairs");
    assert_eq!(report.recovered_paths.len(), 1);
    let row = fixture.reload(&meta).expect("row");
    assert_eq!(row.revision, 2);
    assert_eq!(
        fixture.body_on_disk(&meta).as_deref(),
        Some(NEW),
        "recovery installs the committed content"
    );
    assert_body_matches_metadata(&fixture, &row, context);
    assert_audit_converges(&fixture, context).await;
}

#[tokio::test]
async fn a_failed_metadata_revert_is_reported_never_silently_served() {
    // Double fault: the swap fails, the body is restored, but reverting the metadata fails.
    // The prior body is what is served; the divergence is REPORTED and nothing is deleted.
    let context = "update-error-revert-metadata";
    let faults = Arc::new(
        ScriptedFaults::new()
            .fail_once(FaultPoint::UpdateSwap)
            .fail_once(FaultPoint::UpdateRevertMetadata),
    );
    let (fixture, meta) = staged_update(context, faults.clone()).await;
    fixture
        .store
        .update_for(&meta, html_update(1, NEW), mutation_audit())
        .await
        .expect_err("the metadata revert fails");
    assert!(faults.all_fired());

    assert_eq!(
        fixture.body_on_disk(&meta).as_deref(),
        Some(OLD),
        "the prior body is restored and served"
    );
    let report = fixture
        .store
        .audit_storage(true)
        .await
        .expect("startup audit runs");
    assert_eq!(
        report.divergent_bodies,
        vec![meta.id.0.clone()],
        "the mismatch is reported"
    );
    assert_eq!(
        fixture.body_on_disk(&meta).as_deref(),
        Some(OLD),
        "reconciliation never deletes a genuine body"
    );
}

// ---------------------------------------------------------------------------
// delete
// ---------------------------------------------------------------------------

#[tokio::test]
async fn delete_compensation_puts_the_body_back_while_the_row_lives() {
    for point in [FaultPoint::DeleteTrashRename, FaultPoint::DeleteRow] {
        let context = format!("delete-error-{point:?}");
        let faults = Arc::new(ScriptedFaults::new().fail_once(point));
        let fixture = Fixture::with_faults(&context, faults.clone());
        let meta = fixture.publish_single(OLD).await;

        fixture
            .store
            .delete_for(&meta, mutation_audit())
            .await
            .expect_err("the armed fault aborts the delete");
        assert!(faults.all_fired(), "{context}");

        assert!(
            fixture.reload(&meta).is_some(),
            "{context}: the row survives"
        );
        assert_eq!(
            fixture.body_on_disk(&meta).as_deref(),
            Some(OLD),
            "{context}: the body is moved back out of trash"
        );
        assert!(
            fixture.trash_entries().is_empty(),
            "{context}: no trash is left behind"
        );
        assert_audit_converges(&fixture, &context).await;
    }
}

#[tokio::test]
async fn a_delete_that_fails_after_the_row_is_gone_leaves_a_reported_orphan() {
    // Once SQLite has committed the delete, restoring the body would create an unowned orphan.
    // Keep the durable trash and its intent instead; the deletion is not acknowledged until
    // cleanup succeeds, and startup must not silently delete that recovery evidence.
    let context = "delete-error-trash-remove";
    let faults = Arc::new(ScriptedFaults::new().fail_once(FaultPoint::DeleteTrashRemove));
    let fixture = Fixture::with_faults(context, faults.clone());
    let meta = fixture.publish_single(OLD).await;

    fixture
        .store
        .delete_for(&meta, mutation_audit())
        .await
        .expect_err("the armed fault aborts the delete");
    assert!(faults.all_fired());
    assert!(fixture.reload(&meta).is_none(), "the row is gone");
    assert_eq!(fixture.body_on_disk(&meta), None);
    assert_eq!(
        fixture.trash_entries().len(),
        1,
        "durable trash remains for recovery"
    );

    let report = fixture
        .store
        .audit_storage(true)
        .await
        .expect("startup audit runs");
    assert!(
        report.orphan_bodies.is_empty(),
        "trash is not a public orphan body"
    );
    assert!(
        fixture.trash_entries().is_empty(),
        "startup retries and durably completes cleanup"
    );
}

#[tokio::test]
async fn a_delete_history_cleanup_failure_keeps_its_intent_until_history_is_durable() {
    // This is the failure that must not be silently converted to generic best-effort orphan
    // cleanup: trash removal succeeded, but the SQL delete has already cascaded the revision
    // rows and the history directory still contains the only cleanup evidence.
    let faults = Arc::new(ScriptedFaults::new().fail_once(FaultPoint::DeleteHistoryRemove));
    let fixture = Fixture::with_faults("delete-history-cleanup", faults.clone());
    let original = fixture.publish_single(OLD).await;
    let current = fixture
        .store
        .update_for(&original, html_update(1, NEW), mutation_audit())
        .await
        .expect("history exists before delete")
        .meta;

    fixture
        .store
        .delete_for(&current, mutation_audit())
        .await
        .expect_err("history cleanup fails after the SQL delete");
    assert!(faults.all_fired());
    assert!(fixture.reload(&current).is_none(), "row has been deleted");
    assert!(
        fixture.trash_entries().is_empty(),
        "trash was already removed"
    );
    assert!(
        !fixture.history_entries(&current).is_empty(),
        "history remains and must not become untracked best-effort cleanup"
    );
    assert_eq!(
        fixture.count("SELECT COUNT(*) FROM artifact_durability_intents"),
        1,
        "the delete intent survives until both cleanup barriers have passed"
    );

    fixture
        .store
        .audit_storage(true)
        .await
        .expect("startup retries durable history removal");
    assert!(fixture.history_entries(&current).is_empty());
    assert_eq!(
        fixture.count("SELECT COUNT(*) FROM artifact_durability_intents"),
        0,
        "only a completed history cleanup clears the intent"
    );
    assert_audit_converges(&fixture, "delete-history-cleanup").await;
}

#[tokio::test]
async fn delete_crashes_converge_on_a_consistent_store() {
    // (failpoint, does the artifact still exist after recovery?)
    let cases = [
        (FaultPoint::DeleteTrashRename, true),
        (FaultPoint::DeleteRow, true),
        (FaultPoint::DeleteTrashRemove, false),
        (FaultPoint::DeleteHistoryRemove, false),
    ];
    for (point, survives) in cases {
        let context = format!("delete-crash-{point:?}");
        let faults = Arc::new(ScriptedFaults::new().crash_once(point));
        let fixture = Fixture::with_faults(&context, faults.clone());
        let meta = fixture.publish_single(OLD).await;
        let updated = fixture
            .store
            .update_for(&meta, html_update(1, NEW), mutation_audit())
            .await
            .expect("update creates a history snapshot")
            .meta;

        fixture
            .store
            .delete_for(&updated, mutation_audit())
            .await
            .expect_err("the armed crash aborts the delete");
        assert!(faults.all_fired(), "{context}");

        let report = fixture
            .store
            .audit_storage(true)
            .await
            .expect("startup audit runs");
        if survives {
            let row = fixture
                .reload(&meta)
                .unwrap_or_else(|| panic!("{context}: the row survives"));
            assert_eq!(
                fixture.body_on_disk(&meta).as_deref(),
                Some(NEW),
                "{context}: an interrupted delete puts the body back"
            );
            assert_body_matches_metadata(&fixture, &row, &context);
            assert!(report.missing_bodies.is_empty(), "{context}: {report:?}");
        } else {
            assert!(fixture.reload(&meta).is_none(), "{context}");
            assert!(
                fixture.body_on_disk(&meta).is_none(),
                "{context}: the trashed body is discarded"
            );
            assert!(
                fixture.history_entries(&meta).is_empty(),
                "{context}: orphan history is reclaimed"
            );
        }
        assert!(
            fixture.transient_entries().is_empty(),
            "{context}: reconciliation clears every transient path"
        );
        assert_audit_converges(&fixture, &context).await;
    }
}

#[tokio::test]
async fn a_delete_whose_compensation_fails_is_still_recoverable_at_startup() {
    let context = "delete-error-restore-body";
    let faults = Arc::new(
        ScriptedFaults::new()
            .fail_once(FaultPoint::DeleteRow)
            .fail_once(FaultPoint::DeleteRestoreBody),
    );
    let fixture = Fixture::with_faults(context, faults.clone());
    let meta = fixture.publish_single(OLD).await;

    fixture
        .store
        .delete_for(&meta, mutation_audit())
        .await
        .expect_err("both the delete and its compensation fail");
    assert!(faults.all_fired());
    assert_eq!(
        fixture.count("SELECT COUNT(*) FROM artifacts"),
        1,
        "the row survives (but remains concealed)"
    );
    assert!(
        fixture.body_on_disk(&meta).is_none(),
        "the body is stranded in trash"
    );

    let report = fixture
        .store
        .audit_storage(true)
        .await
        .expect("startup audit repairs");
    assert_eq!(report.recovered_paths.len(), 1);
    assert_eq!(fixture.body_on_disk(&meta).as_deref(), Some(OLD));
    assert!(report.missing_bodies.is_empty());
}

// ---------------------------------------------------------------------------
// re-tenant, metadata writes, reconciliation, backfill
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_failed_re_tenant_moves_nothing() {
    for crash in [false, true] {
        let context = format!("move-fault-crash-{crash}");
        let faults = Arc::new(if crash {
            ScriptedFaults::new().crash_once(FaultPoint::MoveTransaction)
        } else {
            ScriptedFaults::new().fail_once(FaultPoint::MoveTransaction)
        });
        let fixture = Fixture::with_faults(&context, faults.clone());
        fixture.create_org("other");
        let meta = fixture.publish_single(OLD).await;
        fixture.execute(&format!(
            "INSERT INTO artifact_shares (token, artifact_id, org, created_by) \
             VALUES ('share-token-0000000009', '{}', '{TEST_ORG}', 'owner@example.com')",
            meta.id.0
        ));

        fixture
            .store
            .move_to_org_for(&meta, "other", None, mutation_audit())
            .await
            .expect_err("the armed fault aborts the move");
        assert!(faults.all_fired(), "{context}");

        assert_eq!(
            fixture.reload(&meta).map(|row| row.org.0),
            Some(TEST_ORG.to_owned()),
            "{context}: the tenant is unchanged"
        );
        assert_eq!(
            fixture.count("SELECT COUNT(*) FROM artifact_shares"),
            1,
            "{context}: shares are only revoked by a committed move"
        );
        assert_eq!(fixture.foreign_key_violations(), 0, "{context}");
    }
}

#[tokio::test]
async fn a_failed_metadata_write_changes_nothing() {
    let faults = Arc::new(
        ScriptedFaults::new()
            .fail_once(FaultPoint::MetadataWrite)
            .fail_once(FaultPoint::MetadataWrite),
    );
    let fixture = Fixture::with_faults("metadata-write-fault", faults.clone());
    let meta = fixture.publish_single(OLD).await;

    fixture
        .store
        .set_category_for(&meta, "changed", mutation_audit())
        .await
        .expect_err("the armed fault aborts the category write");
    fixture
        .store
        .set_hidden_for(&meta, true, mutation_audit())
        .await
        .expect_err("the armed fault aborts the visibility write");
    assert!(faults.all_fired());

    let row = fixture.reload(&meta).expect("row");
    assert_eq!(row.category, "docs");
    assert!(!row.hidden);
    assert_eq!(row.updated_at, meta.updated_at);
}

#[tokio::test]
async fn a_failed_reconciliation_destroys_nothing_and_retries_cleanly() {
    for point in [
        FaultPoint::ReconcileRecover,
        FaultPoint::ReconcileDiscard,
        FaultPoint::ReconcileOrphanHistory,
    ] {
        let context = format!("reconcile-fault-{point:?}");
        let faults = Arc::new(ScriptedFaults::new().fail_once(point));
        let fixture = Fixture::with_faults(&context, faults.clone());
        let meta = fixture.publish_single(OLD).await;
        // Create work for all three branches: a recoverable staged body, an unreferenced
        // transient path, and an orphan history directory.
        fixture
            .store
            .update_for(&meta, html_update(1, NEW), mutation_audit())
            .await
            .expect("update creates history");
        std::fs::write(
            fixture
                .artifact_dir
                .join(".zzzzzzzzzzzz.staging-aaaaaaaaaaaa"),
            "unreferenced",
        )
        .expect("write unreferenced staging");
        std::fs::create_dir_all(fixture.artifact_dir.join(".history").join("zzzzzzzzzzzz"))
            .expect("create orphan history");
        std::fs::rename(
            fixture.artifact_dir.join(format!("{}.html", meta.id.0)),
            fixture
                .artifact_dir
                .join(format!(".{}.staging-bbbbbbbbbbbb", meta.id.0)),
        )
        .expect("simulate an interrupted swap");

        fixture
            .store
            .audit_storage(true)
            .await
            .expect_err("the armed fault aborts reconciliation");
        assert!(faults.all_fired(), "{context}");

        // Whatever failed, the committed body is still recoverable and nothing was destroyed.
        let report = fixture
            .store
            .audit_storage(true)
            .await
            .expect("a retry completes");
        assert!(report.recovered_paths.len() <= 1, "{context}: {report:?}");
        assert_eq!(
            fixture.body_on_disk(&meta).as_deref(),
            Some(NEW),
            "{context}: the committed body is reinstated"
        );
        assert!(
            fixture.transient_entries().is_empty(),
            "{context}: the retry clears every transient path"
        );
        assert_audit_converges(&fixture, &context).await;
    }
}

#[tokio::test]
async fn a_failed_digest_backfill_leaves_the_row_blank_for_the_next_run() {
    let faults = Arc::new(ScriptedFaults::new().fail_once(FaultPoint::BackfillWrite));
    let fixture = Fixture::with_faults("backfill-fault", faults.clone());
    let meta = fixture.publish_single(OLD).await;
    fixture.execute("UPDATE artifacts SET body_sha256 = ''");

    fixture
        .store
        .backfill_body_digests()
        .await
        .expect_err("the armed fault aborts the backfill");
    assert!(faults.all_fired());
    assert_eq!(fixture.recorded_digest(&meta), "");

    let report = fixture
        .store
        .backfill_body_digests()
        .await
        .expect("a retry completes");
    assert_eq!(report.scanned, 1);
    assert_eq!(report.updated, 1);
    assert_eq!(fixture.recorded_digest(&meta), sha256_hex(OLD));
    assert_eq!(
        fixture.reload(&meta).map(|row| row.revision),
        Some(1),
        "the retry is still metadata-only"
    );
}

// ---------------------------------------------------------------------------
// bundles under fault
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_crashed_bundle_update_recovers_the_whole_directory() {
    let context = "bundle-crash-swap";
    let faults = Arc::new(ScriptedFaults::new().crash_once(FaultPoint::UpdateSwap));
    let fixture = Fixture::with_faults(context, faults.clone());
    let published = fixture
        .publish_bundle(&[("index.html", "one"), ("app.js", "1")], None)
        .await;

    fixture
        .store
        .update_for(
            &published.meta,
            ArtifactUpdate {
                expected_revision: 1,
                content: Some(crate::u08_support::bundle_content(
                    &[("index.html", "two"), ("app.js", "2")],
                    None,
                )),
                ..ArtifactUpdate::default()
            },
            mutation_audit(),
        )
        .await
        .expect_err("crash before the directory swap");
    assert!(faults.all_fired());

    let report = fixture
        .store
        .audit_storage(true)
        .await
        .expect("startup audit repairs");
    assert_eq!(report.recovered_paths.len(), 1);
    assert_eq!(
        fixture
            .bundle_file_on_disk(&published.meta, "index.html")
            .as_deref(),
        Some("two")
    );
    assert_eq!(
        fixture
            .bundle_file_on_disk(&published.meta, "app.js")
            .as_deref(),
        Some("2")
    );
    assert!(report.divergent_bodies.is_empty());
    assert!(report.missing_bodies.is_empty());
}
