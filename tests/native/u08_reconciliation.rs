//! U08 startup reconciliation, driven from **prepared crash pre-states**.
//!
//! `u08_failpoints.rs` reaches these states by interrupting a real operation. This suite builds
//! them directly on disk and in the database, so each ADR-0002 rule is pinned in isolation and a
//! future refactor cannot make a rule untestable by changing when it is reachable.
//!
//! Node oracle: `auditStorage({ cleanTransient })` — [lib/store.js:675-744].

use artifact_mcp::model::ArtifactMeta;
use artifact_mcp::ports::ArtifactService as _;

use crate::u08_support::{Fixture, html_update, mutation_audit, read_names, sha256_hex};

const OLD: &str = "<p>OLD</p>";
const NEW: &str = "<p>NEW</p>";

/// Move the live single-file body to a staging path, as an interrupted swap would leave it.
fn park_as_staging(fixture: &Fixture, meta: &ArtifactMeta, token: &str) {
    std::fs::rename(
        fixture.artifact_dir.join(format!("{}.html", meta.id.0)),
        fixture
            .artifact_dir
            .join(format!(".{}.staging-{token}", meta.id.0)),
    )
    .expect("park the body in staging");
}

/// Write an arbitrary transient body for `meta` without touching the live path.
fn write_transient(fixture: &Fixture, meta: &ArtifactMeta, kind: &str, token: &str, body: &str) {
    std::fs::write(
        fixture
            .artifact_dir
            .join(format!(".{}.{kind}-{token}", meta.id.0)),
        body,
    )
    .expect("write transient body");
}

// ---------------------------------------------------------------------------
// staged bodies
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_staged_body_is_installed_when_the_final_path_is_empty() {
    let fixture = Fixture::new("recon-staged-empty");
    let meta = fixture.publish_single(OLD).await;
    park_as_staging(&fixture, &meta, "aaaaaaaaaaaa");
    assert!(fixture.body_on_disk(&meta).is_none());

    let report = fixture
        .store
        .audit_storage(true)
        .await
        .expect("startup audit");

    assert_eq!(
        report.recovered_paths,
        vec![format!(".{}.staging-aaaaaaaaaaaa", meta.id.0)]
    );
    assert_eq!(fixture.body_on_disk(&meta).as_deref(), Some(OLD));
    assert!(report.missing_bodies.is_empty());
    assert!(report.divergent_bodies.is_empty());
    assert!(fixture.transient_entries().is_empty());
}

#[tokio::test]
async fn a_staged_body_replaces_a_stale_one_when_the_digest_disagrees() {
    // The commit-then-swap window: metadata is already at the NEW digest while the OLD body is
    // still installed and the NEW body waits in staging. [lib/store.js:705-712]
    let fixture = Fixture::new("recon-staged-stale");
    let meta = fixture.publish_single(OLD).await;
    fixture.execute(&format!(
        "UPDATE artifacts SET body_sha256 = '{}', revision = 2 WHERE id = '{}'",
        sha256_hex(NEW),
        meta.id.0
    ));
    write_transient(&fixture, &meta, "staging", "bbbbbbbbbbbb", NEW);

    let report = fixture
        .store
        .audit_storage(true)
        .await
        .expect("startup audit");

    assert_eq!(report.recovered_paths.len(), 1);
    assert_eq!(
        fixture.body_on_disk(&meta).as_deref(),
        Some(NEW),
        "the committed content wins"
    );
    assert!(
        report.divergent_bodies.is_empty(),
        "and the divergence is resolved, not merely reported"
    );
    assert!(fixture.transient_entries().is_empty());
}

#[tokio::test]
async fn a_truncated_staged_body_is_preserved_and_reported_without_replacing_the_final_body() {
    let fixture = Fixture::new("recon-corrupt-staging");
    let meta = fixture.publish_single(OLD).await;
    fixture.execute(&format!(
        "UPDATE artifacts SET body_sha256 = '{}', revision = 2 WHERE id = '{}'",
        sha256_hex(NEW),
        meta.id.0
    ));
    let staging_name = format!(".{}.staging-badbadbadbad", meta.id.0);
    write_transient(&fixture, &meta, "staging", "badbadbadbad", "<p>NEW");

    let report = fixture
        .store
        .audit_storage(true)
        .await
        .expect("startup audit");

    assert_eq!(
        fixture.body_on_disk(&meta).as_deref(),
        Some(OLD),
        "recovery must not replace the intact final body with truncated staging"
    );
    assert!(
        fixture.artifact_dir.join(&staging_name).exists(),
        "the truncated staging path is preserved for inspection or manual recovery"
    );
    assert_eq!(
        std::fs::read_to_string(fixture.artifact_dir.join(&staging_name))
            .expect("read preserved staging body"),
        "<p>NEW"
    );
    assert_eq!(report.transient_paths, vec![staging_name.clone()]);
    assert!(report.recovered_paths.is_empty());
    assert_eq!(report.divergent_bodies, vec![meta.id.0.clone()]);
}

#[tokio::test]
async fn a_staged_body_is_discarded_when_the_installed_body_already_matches() {
    let fixture = Fixture::new("recon-staged-matching");
    let meta = fixture.publish_single(OLD).await;
    write_transient(&fixture, &meta, "staging", "cccccccccccc", "abandoned");

    let report = fixture
        .store
        .audit_storage(true)
        .await
        .expect("startup audit");

    assert!(report.recovered_paths.is_empty());
    assert_eq!(report.transient_paths.len(), 1);
    assert_eq!(
        fixture.body_on_disk(&meta).as_deref(),
        Some(OLD),
        "the committed body is left alone"
    );
    assert!(fixture.transient_entries().is_empty());
}

// ---------------------------------------------------------------------------
// trashed bodies
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_trashed_body_returns_when_its_record_survived_the_crash() {
    let fixture = Fixture::new("recon-trash-row-lives");
    let meta = fixture.publish_single(OLD).await;
    std::fs::rename(
        fixture.artifact_dir.join(format!("{}.html", meta.id.0)),
        fixture
            .artifact_dir
            .join(format!(".{}.trash-dddddddddddd", meta.id.0)),
    )
    .expect("park the body in trash");

    let report = fixture
        .store
        .audit_storage(true)
        .await
        .expect("startup audit");

    assert_eq!(report.recovered_paths.len(), 1);
    assert_eq!(fixture.body_on_disk(&meta).as_deref(), Some(OLD));
    assert!(report.missing_bodies.is_empty());
}

#[tokio::test]
async fn a_trashed_body_is_discarded_once_its_record_is_gone() {
    let fixture = Fixture::new("recon-trash-row-gone");
    let meta = fixture.publish_single(OLD).await;
    std::fs::rename(
        fixture.artifact_dir.join(format!("{}.html", meta.id.0)),
        fixture
            .artifact_dir
            .join(format!(".{}.trash-eeeeeeeeeeee", meta.id.0)),
    )
    .expect("park the body in trash");
    fixture.execute("DELETE FROM artifacts");

    let report = fixture
        .store
        .audit_storage(true)
        .await
        .expect("startup audit");

    assert_eq!(report.transient_paths.len(), 1);
    assert!(report.recovered_paths.is_empty());
    assert!(report.orphan_bodies.is_empty());
    assert!(fixture.entries().iter().all(|name| name == ".history"));
}

#[tokio::test]
async fn a_surviving_staging_path_beats_a_surviving_trash_path() {
    // Both a staged (committed-new) and a trashed (old) body survive one crash. `.staging-` is
    // processed first, so the committed content is installed and the old body is discarded —
    // never the other way around. [lib/store.js:688-691]
    let fixture = Fixture::new("recon-staging-beats-trash");
    let meta = fixture.publish_single(OLD).await;
    std::fs::remove_file(fixture.artifact_dir.join(format!("{}.html", meta.id.0)))
        .expect("clear the final path");
    fixture.execute(&format!(
        "UPDATE artifacts SET body_sha256 = '{}' WHERE id = '{}'",
        sha256_hex(NEW),
        meta.id.0
    ));
    // The trash token sorts before the staging token, so only the rank can decide the winner.
    write_transient(&fixture, &meta, "trash", "aaaaaaaaaaaa", OLD);
    write_transient(&fixture, &meta, "staging", "zzzzzzzzzzzz", NEW);

    let report = fixture
        .store
        .audit_storage(true)
        .await
        .expect("startup audit");

    assert_eq!(
        report.recovered_paths,
        vec![format!(".{}.staging-zzzzzzzzzzzz", meta.id.0)]
    );
    assert_eq!(fixture.body_on_disk(&meta).as_deref(), Some(NEW));
    assert!(fixture.transient_entries().is_empty());
    assert!(report.divergent_bodies.is_empty());
}

#[tokio::test]
async fn an_unreferenced_transient_path_is_removed() {
    let fixture = Fixture::new("recon-unreferenced");
    let meta = fixture.publish_single(OLD).await;
    std::fs::write(
        fixture
            .artifact_dir
            .join(".zzzzzzzzzzzz.staging-ffffffffffff"),
        "nobody owns this",
    )
    .expect("write unreferenced staging");

    let report = fixture
        .store
        .audit_storage(true)
        .await
        .expect("startup audit");

    assert_eq!(report.transient_paths.len(), 1);
    assert!(report.recovered_paths.is_empty());
    assert!(report.orphan_bodies.is_empty(), "{report:?}");
    assert_eq!(fixture.body_on_disk(&meta).as_deref(), Some(OLD));
    assert!(fixture.transient_entries().is_empty());
}

// ---------------------------------------------------------------------------
// genuine divergence: reported, never repaired
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_record_without_a_body_is_reported_and_left_alone() {
    let fixture = Fixture::new("recon-missing-body");
    let meta = fixture.publish_single(OLD).await;
    std::fs::remove_file(fixture.artifact_dir.join(format!("{}.html", meta.id.0)))
        .expect("remove the body");

    let report = fixture
        .store
        .audit_storage(true)
        .await
        .expect("startup audit");

    assert_eq!(report.missing_bodies, vec![meta.id.0.clone()]);
    assert!(
        fixture.reload(&meta).is_some(),
        "reconciliation never deletes a record"
    );
    assert!(report.recovered_paths.is_empty());
}

#[tokio::test]
async fn a_body_without_a_record_is_reported_and_never_deleted() {
    let fixture = Fixture::new("recon-orphan-body");
    let meta = fixture.publish_single(OLD).await;
    fixture.execute("DELETE FROM artifacts");

    let report = fixture
        .store
        .audit_storage(true)
        .await
        .expect("startup audit");

    assert_eq!(report.orphan_bodies, vec![format!("{}.html", meta.id.0)]);
    assert_eq!(
        fixture.body_on_disk(&meta).as_deref(),
        Some(OLD),
        "ADR-0002: destructive reconciliation requires an explicit future decision"
    );
}

#[tokio::test]
async fn a_divergent_body_is_reported_and_never_replaced() {
    let fixture = Fixture::new("recon-divergent");
    let meta = fixture.publish_single(OLD).await;
    std::fs::write(
        fixture.artifact_dir.join(format!("{}.html", meta.id.0)),
        "hand edited",
    )
    .expect("diverge the body");

    let report = fixture
        .store
        .audit_storage(true)
        .await
        .expect("startup audit");

    assert_eq!(report.divergent_bodies, vec![meta.id.0.clone()]);
    assert_eq!(fixture.body_on_disk(&meta).as_deref(), Some("hand edited"));
    assert!(report.missing_bodies.is_empty());
}

#[tokio::test]
async fn a_blank_legacy_digest_is_never_reported_as_divergent() {
    // Rows predating the v17 migration carry an empty digest; only the backfill fixes those.
    let fixture = Fixture::new("recon-blank-digest");
    let meta = fixture.publish_single(OLD).await;
    fixture.execute("UPDATE artifacts SET body_sha256 = ''");

    let report = fixture
        .store
        .audit_storage(true)
        .await
        .expect("startup audit");

    assert!(report.divergent_bodies.is_empty(), "{report:?}");
    assert_eq!(fixture.body_on_disk(&meta).as_deref(), Some(OLD));
}

// ---------------------------------------------------------------------------
// history
// ---------------------------------------------------------------------------

#[tokio::test]
async fn orphan_history_is_reclaimed_only_when_cleaning() {
    let fixture = Fixture::new("recon-orphan-history");
    let meta = fixture.publish_single(OLD).await;
    fixture
        .store
        .update_for(&meta, html_update(1, NEW), mutation_audit())
        .await
        .expect("update creates a history snapshot");
    assert_eq!(fixture.history_entries(&meta), vec!["1.html"]);
    // Simulate a crash between the row delete and `removeHistory`.
    fixture.execute("DELETE FROM artifacts");
    std::fs::remove_file(fixture.artifact_dir.join(format!("{}.html", meta.id.0)))
        .expect("remove the live body");

    let observed = fixture
        .store
        .audit_storage(false)
        .await
        .expect("read-only audit");
    assert!(observed.orphan_history.is_empty());
    assert_eq!(fixture.history_entries(&meta), vec!["1.html"]);

    let repaired = fixture
        .store
        .audit_storage(true)
        .await
        .expect("startup audit");
    assert_eq!(repaired.orphan_history, vec![meta.id.0.clone()]);
    assert!(fixture.history_entries(&meta).is_empty());
}

#[tokio::test]
async fn the_history_directory_is_never_an_orphan_body() {
    let fixture = Fixture::new("recon-history-not-orphan");
    let meta = fixture.publish_single(OLD).await;
    fixture
        .store
        .update_for(&meta, html_update(1, NEW), mutation_audit())
        .await
        .expect("update creates a history snapshot");

    let report = fixture
        .store
        .audit_storage(true)
        .await
        .expect("startup audit");

    assert!(report.orphan_bodies.is_empty(), "{report:?}");
    assert!(report.orphan_history.is_empty(), "{report:?}");
    assert_eq!(fixture.history_entries(&meta), vec!["1.html"]);
    assert!(
        read_names(&fixture.artifact_dir).contains(&".history".to_owned()),
        "the history store survives reconciliation"
    );
}

// ---------------------------------------------------------------------------
// read-only mode
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_read_only_audit_reports_without_touching_anything() {
    let fixture = Fixture::new("recon-read-only");
    let meta = fixture.publish_single(OLD).await;
    write_transient(&fixture, &meta, "staging", "gggggggggggg", NEW);
    std::fs::write(fixture.artifact_dir.join("qqqqqqqqqqqq.html"), "stray")
        .expect("write an orphan body");
    let before = fixture.entries();

    let report = fixture
        .store
        .audit_storage(false)
        .await
        .expect("read-only audit");

    assert_eq!(report.transient_paths.len(), 1);
    assert!(report.recovered_paths.is_empty());
    assert_eq!(report.orphan_bodies, vec!["qqqqqqqqqqqq.html".to_owned()]);
    assert_eq!(fixture.entries(), before, "nothing was created or removed");
}

#[tokio::test]
async fn reconciliation_of_an_untouched_store_is_a_no_op() {
    let fixture = Fixture::new("recon-clean");
    let meta = fixture.publish_single(OLD).await;
    let published = fixture.publish_bundle(&[("index.html", "one")], None).await;

    let report = fixture
        .store
        .audit_storage(true)
        .await
        .expect("startup audit");

    assert_eq!(
        report,
        Default::default(),
        "a healthy store reports nothing"
    );
    assert_eq!(fixture.body_on_disk(&meta).as_deref(), Some(OLD));
    assert_eq!(
        fixture
            .bundle_file_on_disk(&published.meta, "index.html")
            .as_deref(),
        Some("one")
    );
}
