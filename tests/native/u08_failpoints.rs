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

use std::sync::Arc;

use artifact_mcp::artifacts::lifecycle::{FaultPoint, ScriptedFaults};
use artifact_mcp::error::AppError;
use artifact_mcp::model::{ArtifactContent, ArtifactMeta, ArtifactUpdate, OrgId};
use artifact_mcp::ports::ArtifactService as _;

use crate::u08_support::{Fixture, TEST_ORG, html_update, sha256_hex};

const OLD: &str = "<p>OLD</p>";
const NEW: &str = "<p>NEW-and-longer</p>";

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

const PUBLISH_POINTS: [FaultPoint; 4] = [
    FaultPoint::PublishStageWrite,
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

    let row = sole_artifact(&fixture)
        .await
        .expect("the row deletion failed");
    assert!(
        fixture.body_on_disk(&row).is_none(),
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
            .update_for(&meta, html_update(1, NEW))
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
async fn a_fault_after_the_swap_leaves_a_fully_committed_update() {
    // History pruning is best effort; failing there must not undo a committed revision.
    let context = "update-error-prune";
    let faults = Arc::new(ScriptedFaults::new().fail_once(FaultPoint::UpdatePrune));
    let (fixture, meta) = staged_update(context, faults.clone()).await;

    fixture
        .store
        .update_for(&meta, html_update(1, NEW))
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
            .update_for(&meta, html_update(1, NEW))
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
                .restore_for(&row, 1, None)
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
        .update_for(&meta, html_update(1, NEW))
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
        .restore_for(&current, 1, None)
        .await
        .expect("revision 1 remains restorable");
    assert_eq!(restored.restored_from, 1);
    assert_eq!(restored.meta.revision, 3);
    assert_eq!(fixture.body_on_disk(&meta).as_deref(), Some(OLD));
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
        .update_for(&meta, html_update(1, NEW))
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
        .update_for(&meta, html_update(1, NEW))
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
            .delete_for(&meta)
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
    // Node restores the body on any throw after the row delete, which produces an orphan body.
    // ADR-0002 is explicit that orphan bodies are reported, never deleted automatically.
    let context = "delete-error-trash-remove";
    let faults = Arc::new(ScriptedFaults::new().fail_once(FaultPoint::DeleteTrashRemove));
    let fixture = Fixture::with_faults(context, faults.clone());
    let meta = fixture.publish_single(OLD).await;

    fixture
        .store
        .delete_for(&meta)
        .await
        .expect_err("the armed fault aborts the delete");
    assert!(faults.all_fired());
    assert!(fixture.reload(&meta).is_none(), "the row is gone");
    assert_eq!(fixture.body_on_disk(&meta).as_deref(), Some(OLD));

    let report = fixture
        .store
        .audit_storage(true)
        .await
        .expect("startup audit runs");
    assert_eq!(
        report.orphan_bodies,
        vec![format!("{}.html", meta.id.0)],
        "the orphan is reported"
    );
    assert_eq!(
        fixture.body_on_disk(&meta).as_deref(),
        Some(OLD),
        "and never deleted"
    );
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
            .update_for(&meta, html_update(1, NEW))
            .await
            .expect("update creates a history snapshot")
            .meta;

        fixture
            .store
            .delete_for(&updated)
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
        .delete_for(&meta)
        .await
        .expect_err("both the delete and its compensation fail");
    assert!(faults.all_fired());
    assert!(fixture.reload(&meta).is_some(), "the row survives");
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
            .move_to_org_for(&meta, "other", None)
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
        .set_category_for(&meta, "changed")
        .await
        .expect_err("the armed fault aborts the category write");
    fixture
        .store
        .set_hidden_for(&meta, true)
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
            .update_for(&meta, html_update(1, NEW))
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
