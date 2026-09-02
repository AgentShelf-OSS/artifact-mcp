//! U08 lifecycle semantics: publish, update, restore, delete, re-tenant, and digest backfill.
//!
//! These are the *happy* and *rejected* paths. Crash safety lives in `u08_failpoints.rs` and
//! `u08_reconciliation.rs`; cross-runtime agreement lives in `u08_node_parity.rs`.
//!
//! Node oracle: `lib/store.js`.

use artifact_mcp::artifacts::digest::bundle_manifest_digest;
use artifact_mcp::config::StorageLimits;
use artifact_mcp::error::AppError;
use artifact_mcp::model::{ArtifactContent, ArtifactId, ArtifactUpdate};
use artifact_mcp::ports::ArtifactService as _;

use crate::u08_support::{
    Fixture, TEST_CLIENT, TEST_ORG, bundle_content, bundle_update, html_update, mutation_audit,
    sha256_hex,
};

// ---------------------------------------------------------------------------
// publish
// ---------------------------------------------------------------------------

#[tokio::test]
async fn publish_installs_the_body_and_commits_matching_metadata() {
    let fixture = Fixture::new("publish-single");
    let meta = fixture.publish_single("<h1>one</h1>").await;

    assert_eq!(meta.revision, 1);
    assert_eq!(meta.bytes, "<h1>one</h1>".len() as u64);
    assert_eq!(meta.body_sha256, sha256_hex("<h1>one</h1>"));
    assert_eq!(meta.org.0, TEST_ORG);
    assert_eq!(meta.client_id.0, TEST_CLIENT);
    assert!(!meta.is_bundle);
    assert_eq!(meta.entry, "");
    assert_eq!(meta.category, "docs");

    assert_eq!(
        fixture.body_on_disk(&meta).as_deref(),
        Some("<h1>one</h1>"),
        "the body must be installed at the final path"
    );
    assert!(
        fixture.transient_entries().is_empty(),
        "a successful publish leaves no staging path: {:?}",
        fixture.entries()
    );
}

#[tokio::test]
async fn publish_bundle_selects_the_first_html_in_publisher_order() {
    // Contract delta 4: `{z.html, a.html}` with no index.html selects z.html, because
    // auto-selection walks the publisher's insertion order. [lib/store.js:254]
    let fixture = Fixture::new("publish-bundle-order");
    let published = fixture
        .publish_bundle(&[("z.html", "<b>z</b>"), ("a.html", "<b>a</b>")], None)
        .await;

    assert_eq!(published.meta.entry, "z.html");
    assert_eq!(published.file_count, Some(2));
    assert!(published.meta.is_bundle);
    assert_eq!(
        published.meta.bytes,
        ("<b>z</b>".len() + "<b>a</b>".len()) as u64
    );
    assert_eq!(
        published.meta.body_sha256,
        bundle_manifest_digest(&[
            ("z.html".to_owned(), "<b>z</b>".to_owned()),
            ("a.html".to_owned(), "<b>a</b>".to_owned()),
        ])
    );
    assert_eq!(
        fixture
            .bundle_file_on_disk(&published.meta, "a.html")
            .as_deref(),
        Some("<b>a</b>")
    );
}

#[tokio::test]
async fn publish_bundle_prefers_index_html_then_an_explicit_entry() {
    let fixture = Fixture::new("publish-bundle-entry");
    let implicit = fixture
        .publish_bundle(&[("z.html", "z"), ("index.html", "i")], None)
        .await;
    assert_eq!(implicit.meta.entry, "index.html");

    let explicit = fixture
        .publish_bundle(&[("z.html", "z"), ("index.html", "i")], Some("z.html"))
        .await;
    assert_eq!(explicit.meta.entry, "z.html");
}

#[tokio::test]
async fn publish_reports_nodes_validation_messages() {
    let fixture = Fixture::new("publish-validation");

    let blank = fixture
        .try_publish(ArtifactContent::SingleHtml("   ".to_owned()))
        .await
        .expect_err("blank html is rejected");
    assert_eq!(blank, AppError::Validation("html is required".to_owned()));

    let empty_bundle = fixture
        .try_publish(bundle_content(&[], None))
        .await
        .expect_err("an empty bundle is rejected");
    assert_eq!(
        empty_bundle,
        AppError::Validation("files is empty".to_owned())
    );

    let no_entry = fixture
        .try_publish(bundle_content(&[("style.css", "body{}")], None))
        .await
        .expect_err("a bundle with no HTML is rejected");
    assert_eq!(
        no_entry,
        AppError::Validation(
            "no HTML entry found — include index.html or pass an 'entry'".to_owned()
        )
    );

    let bad_entry = fixture
        .try_publish(bundle_content(&[("a.html", "a")], Some("missing.html")))
        .await
        .expect_err("an unknown entry is rejected");
    assert_eq!(
        bad_entry,
        AppError::Validation("entry \"missing.html\" is not one of the files".to_owned())
    );

    assert!(
        fixture.entries().is_empty(),
        "a rejected publish writes nothing: {:?}",
        fixture.entries()
    );
    assert_eq!(fixture.count("SELECT COUNT(*) FROM artifacts"), 0);
}

// ---------------------------------------------------------------------------
// update
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_exact_no_op_update_creates_no_revision() {
    let fixture = Fixture::new("update-noop");
    let meta = fixture.publish_single("<p>same</p>").await;

    let result = fixture
        .store
        .update_for(
            &meta,
            ArtifactUpdate {
                expected_revision: 1,
                title: Some(meta.title.clone()),
                description: Some(meta.description.clone()),
                category: Some(meta.category.clone()),
                content: Some(ArtifactContent::SingleHtml("<p>same</p>".to_owned())),
                acting_client_id: None,
            },
            mutation_audit(),
        )
        .await
        .expect("a no-op update succeeds");

    assert!(!result.changed);
    assert_eq!(result.meta.revision, 1);
    assert_eq!(
        fixture.count("SELECT COUNT(*) FROM artifact_revisions"),
        1,
        "only the live attribution marker"
    );
    assert!(
        fixture.history_entries(&meta).is_empty(),
        "no history snapshot"
    );
    assert!(
        fixture.transient_entries().is_empty(),
        "no staged body left behind"
    );
    assert_eq!(
        fixture.reload(&meta).map(|row| row.updated_at),
        Some(meta.updated_at.clone()),
        "updated_at is untouched"
    );
}

#[tokio::test]
async fn update_bumps_the_revision_and_snapshots_the_outgoing_body() {
    let fixture = Fixture::new("update-body");
    let meta = fixture.publish_single("OLD").await;

    let result = fixture
        .store
        .update_for(&meta, html_update(1, "NEW"), mutation_audit())
        .await
        .expect("update succeeds");

    assert!(result.changed);
    assert_eq!(result.meta.revision, 2);
    assert_eq!(result.meta.body_sha256, sha256_hex("NEW"));
    assert_eq!(fixture.body_on_disk(&meta).as_deref(), Some("NEW"));
    // The OUTGOING revision (1) is what gets recorded and snapshotted. [lib/store.js:413]
    assert_eq!(
        fixture.scalar::<i64>("SELECT revision FROM artifact_revisions WHERE revision = 1"),
        1
    );
    assert_eq!(
        fixture.scalar::<String>("SELECT body_sha256 FROM artifact_revisions WHERE revision = 1"),
        sha256_hex("OLD")
    );
    assert_eq!(fixture.history_body(&meta, 1).as_deref(), Some("OLD"));
    assert!(fixture.transient_entries().is_empty());
}

#[tokio::test]
async fn update_rejects_a_stale_expected_revision() {
    let fixture = Fixture::new("update-conflict");
    let meta = fixture.publish_single("OLD").await;
    fixture
        .store
        .update_for(&meta, html_update(1, "NEW"), mutation_audit())
        .await
        .expect("first update succeeds");

    // `meta` still carries revision 1, which is now stale.
    let conflict = fixture
        .store
        .update_for(&meta, html_update(1, "NEWER"), mutation_audit())
        .await
        .expect_err("a stale revision conflicts");
    assert_eq!(conflict, AppError::Conflict("conflict".to_owned()));
    assert_eq!(fixture.body_on_disk(&meta).as_deref(), Some("NEW"));
    assert!(
        fixture.transient_entries().is_empty(),
        "a conflicting update removes its staged body: {:?}",
        fixture.entries()
    );
    // A zero expected revision is Node's `< 1` rejection. [lib/store.js:319-321]
    let invalid = fixture
        .store
        .update_for(&meta, html_update(0, "NEWER"), mutation_audit())
        .await
        .expect_err("revision 0 conflicts");
    assert_eq!(invalid, AppError::Conflict("conflict".to_owned()));
}

#[tokio::test]
async fn a_metadata_only_update_copies_the_body_and_keeps_it_live() {
    let fixture = Fixture::new("update-metadata");
    let meta = fixture.publish_single("BODY").await;

    let result = fixture
        .store
        .update_for(
            &meta,
            ArtifactUpdate {
                expected_revision: 1,
                title: Some("Renamed".to_owned()),
                ..ArtifactUpdate::default()
            },
            mutation_audit(),
        )
        .await
        .expect("metadata update succeeds");

    assert!(result.changed);
    assert_eq!(result.meta.revision, 2);
    assert_eq!(result.meta.title, "Renamed");
    assert_eq!(result.meta.body_sha256, meta.body_sha256);
    assert_eq!(
        fixture.body_on_disk(&meta).as_deref(),
        Some("BODY"),
        "the live body stays in place"
    );
    assert_eq!(
        fixture.history_body(&meta, 1).as_deref(),
        Some("BODY"),
        "and is copied, not moved, into history"
    );
}

#[tokio::test]
async fn an_entry_only_bundle_update_revisions_without_replacing_files() {
    let fixture = Fixture::new("update-entry-only");
    let published = fixture
        .publish_bundle(&[("index.html", "I"), ("other.html", "O")], None)
        .await;
    assert_eq!(published.meta.entry, "index.html");

    let result = fixture
        .store
        .update_for(
            &published.meta,
            ArtifactUpdate {
                expected_revision: 1,
                content: Some(bundle_content(&[], Some("other.html"))),
                ..ArtifactUpdate::default()
            },
            mutation_audit(),
        )
        .await
        .expect("entry-only update succeeds");

    assert!(result.changed);
    assert_eq!(result.meta.revision, 2);
    assert_eq!(result.meta.entry, "other.html");
    assert_eq!(result.meta.body_sha256, published.meta.body_sha256);
    assert_eq!(
        fixture
            .bundle_file_on_disk(&published.meta, "index.html")
            .as_deref(),
        Some("I"),
        "the live directory is untouched"
    );

    let missing = fixture
        .store
        .update_for(
            &result.meta,
            ArtifactUpdate {
                expected_revision: 2,
                content: Some(bundle_content(&[], Some("nope.html"))),
                ..ArtifactUpdate::default()
            },
            mutation_audit(),
        )
        .await
        .expect_err("an entry that is not a file is rejected");
    assert_eq!(
        missing,
        AppError::Validation("entry \"nope.html\" is not one of the files".to_owned())
    );
}

#[tokio::test]
async fn update_refuses_to_change_the_artifact_shape() {
    let fixture = Fixture::new("update-shape");
    let single = fixture.publish_single("S").await;
    let bundle = fixture
        .publish_bundle(&[("index.html", "I")], None)
        .await
        .meta;

    assert_eq!(
        fixture
            .store
            .update_for(
                &single,
                bundle_update(1, &[("index.html", "I")], None),
                mutation_audit()
            )
            .await
            .expect_err("a bundle payload on a single-file artifact is rejected"),
        AppError::Validation("artifact is single-file; pass html, not files".to_owned())
    );
    assert_eq!(
        fixture
            .store
            .update_for(&bundle, html_update(1, "S"), mutation_audit())
            .await
            .expect_err("an html payload on a bundle is rejected"),
        AppError::Validation("artifact is a bundle; pass files, not html".to_owned())
    );
    assert_eq!(
        fixture
            .store
            .update_for(
                &single,
                ArtifactUpdate {
                    expected_revision: 1,
                    content: Some(bundle_content(&[], Some("index.html"))),
                    ..ArtifactUpdate::default()
                },
                mutation_audit()
            )
            .await
            .expect_err("an entry on a single-file artifact is rejected"),
        AppError::Validation("artifact is single-file; entry only applies to bundles".to_owned())
    );
}

// ---------------------------------------------------------------------------
// history retention and restore
// ---------------------------------------------------------------------------

#[tokio::test]
async fn history_retention_keeps_only_the_newest_snapshots() {
    let limits = StorageLimits {
        max_history: 2,
        ..StorageLimits::default()
    };
    let fixture = Fixture::with_limits("history-prune", limits);
    let meta = fixture.publish_single("v1").await;

    let mut current = meta.clone();
    for version in 2..=5 {
        current = fixture
            .store
            .update_for(
                &current,
                html_update(current.revision, &format!("v{version}")),
                mutation_audit(),
            )
            .await
            .expect("update succeeds")
            .meta;
    }
    assert_eq!(current.revision, 5);

    let retained: Vec<i64> = {
        let conn = artifact_mcp::persistence::db::checkout(&fixture.pool).expect("checkout");
        let mut statement = conn
            .prepare("SELECT revision FROM artifact_revisions ORDER BY revision DESC")
            .expect("prepare");
        let rows = statement
            .query_map([], |row| row.get::<_, i64>(0))
            .expect("query");
        rows.collect::<rusqlite::Result<Vec<_>>>().expect("collect")
    };
    assert_eq!(
        retained,
        vec![5, 4, 3],
        "the live attribution marker plus MAX_HISTORY snapshots survive"
    );
    assert_eq!(fixture.history_entries(&meta), vec!["3.html", "4.html"]);
}

#[tokio::test]
async fn restore_replays_a_past_revision_as_a_new_revision() {
    let fixture = Fixture::new("restore");
    let meta = fixture.publish_single("v1").await;
    let v2 = fixture
        .store
        .update_for(&meta, html_update(1, "v2"), mutation_audit())
        .await
        .expect("update succeeds")
        .meta;
    assert_eq!(v2.revision, 2);

    let restored = fixture
        .store
        .restore_for(&v2, 1, None, mutation_audit())
        .await
        .expect("restore succeeds");

    assert_eq!(restored.restored_from, 1);
    assert_eq!(restored.meta.revision, 3, "restore is append-only");
    assert_eq!(fixture.body_on_disk(&meta).as_deref(), Some("v1"));
    // The restore is itself undoable: revision 2's body was snapshotted on the way out.
    assert_eq!(fixture.history_body(&meta, 2).as_deref(), Some("v2"));

    let history = fixture
        .store
        .list_revisions_for(&restored.meta)
        .await
        .expect("history reads");
    assert_eq!(history.current, 3);
    assert_eq!(
        history
            .revisions
            .iter()
            .map(|row| row.revision)
            .collect::<Vec<_>>(),
        vec![2, 1],
        "newest first"
    );
}

#[tokio::test]
async fn restore_reports_nodes_failure_reasons() {
    let fixture = Fixture::new("restore-reasons");
    let meta = fixture.publish_single("v1").await;

    assert_eq!(
        fixture
            .store
            .restore_for(&meta, 9, None, mutation_audit())
            .await
            .expect_err("an unknown revision"),
        AppError::NotFound("revision_not_found".to_owned())
    );

    let v2 = fixture
        .store
        .update_for(&meta, html_update(1, "v2"), mutation_audit())
        .await
        .expect("update")
        .meta;
    // Drop the retained body without dropping the row — the `410 Gone` case.
    std::fs::remove_file(
        fixture
            .artifact_dir
            .join(".history")
            .join(&meta.id.0)
            .join("1.html"),
    )
    .expect("remove history body");
    assert_eq!(
        fixture
            .store
            .restore_for(&v2, 1, None, mutation_audit())
            .await
            .expect_err("a dropped body"),
        AppError::Gone("body_missing".to_owned())
    );

    // A bundle revision on a single-file artifact is a type mismatch.
    fixture.execute(&format!(
        "UPDATE artifact_revisions SET is_bundle = 1 WHERE artifact_id = '{}'",
        meta.id.0
    ));
    std::fs::create_dir_all(
        fixture
            .artifact_dir
            .join(".history")
            .join(&meta.id.0)
            .join("1"),
    )
    .expect("create bundle snapshot");
    assert_eq!(
        fixture
            .store
            .restore_for(&v2, 1, None, mutation_audit())
            .await
            .expect_err("a type mismatch"),
        AppError::Conflict("type_mismatch".to_owned())
    );
}

#[tokio::test]
async fn restore_round_trips_a_bundle_snapshot() {
    let fixture = Fixture::new("restore-bundle");
    let published = fixture
        .publish_bundle(&[("index.html", "one"), ("app.js", "1")], None)
        .await;
    let v2 = fixture
        .store
        .update_for(
            &published.meta,
            bundle_update(1, &[("index.html", "two"), ("app.js", "2")], None),
            mutation_audit(),
        )
        .await
        .expect("update")
        .meta;

    let restored = fixture
        .store
        .restore_for(&v2, 1, None, mutation_audit())
        .await
        .expect("restore succeeds");
    assert_eq!(restored.restored_from, 1);
    assert_eq!(restored.meta.revision, 3);
    assert_eq!(restored.meta.body_sha256, published.meta.body_sha256);
    assert_eq!(
        fixture
            .bundle_file_on_disk(&published.meta, "index.html")
            .as_deref(),
        Some("one")
    );
    assert_eq!(
        fixture
            .bundle_file_on_disk(&published.meta, "app.js")
            .as_deref(),
        Some("1")
    );
}

// ---------------------------------------------------------------------------
// delete
// ---------------------------------------------------------------------------

#[tokio::test]
async fn delete_cascades_every_subordinate_table_and_removes_the_bodies() {
    let fixture = Fixture::new("delete-cascade");
    let meta = fixture.publish_single("v1").await;
    let v2 = fixture
        .store
        .update_for(&meta, html_update(1, "v2"), mutation_audit())
        .await
        .expect("update")
        .meta;
    seed_engagement(&fixture, &meta.id);
    assert_eq!(fixture.count("SELECT COUNT(*) FROM reactions"), 1);
    assert_eq!(fixture.count("SELECT COUNT(*) FROM feedback"), 1);
    assert_eq!(fixture.count("SELECT COUNT(*) FROM artifact_views"), 1);
    assert_eq!(fixture.count("SELECT COUNT(*) FROM artifact_shares"), 1);
    assert_eq!(fixture.count("SELECT COUNT(*) FROM artifact_revisions"), 2);

    assert!(
        fixture
            .store
            .delete_for(&v2, mutation_audit())
            .await
            .expect("delete succeeds")
    );

    assert_eq!(fixture.count("SELECT COUNT(*) FROM artifacts"), 0);
    assert_eq!(fixture.count("SELECT COUNT(*) FROM reactions"), 0);
    assert_eq!(fixture.count("SELECT COUNT(*) FROM feedback"), 0);
    assert_eq!(fixture.count("SELECT COUNT(*) FROM artifact_views"), 0);
    assert_eq!(fixture.count("SELECT COUNT(*) FROM artifact_shares"), 0);
    assert_eq!(fixture.count("SELECT COUNT(*) FROM artifact_revisions"), 0);
    assert!(fixture.body_on_disk(&meta).is_none());
    assert_eq!(
        fixture.scalar::<String>(
            "SELECT operation || ':' || result FROM security_audit_events ORDER BY sequence DESC LIMIT 1",
        ),
        "artifact.delete:success",
        "a durable delete has exactly one terminal audited outcome"
    );
    assert_eq!(
        fixture.count("SELECT COUNT(*) FROM security_audit_receipts WHERE state = 'pending'"),
        0,
        "the finalized delete cannot leave an orphan pending receipt"
    );
    assert!(
        fixture.history_entries(&meta).is_empty(),
        "history bodies are removed with the record"
    );
    assert!(
        fixture.entries().iter().all(|name| name == ".history"),
        "no trash survives a successful delete: {:?}",
        fixture.entries()
    );
}

#[tokio::test]
async fn deleting_an_unknown_artifact_reports_false() {
    let fixture = Fixture::new("delete-missing");
    let meta = fixture.publish_single("v1").await;
    assert!(
        fixture
            .store
            .delete_for(&meta, mutation_audit())
            .await
            .expect("first delete")
    );
    assert!(
        !fixture
            .store
            .delete_for(&meta, mutation_audit())
            .await
            .expect("second delete is a no-op")
    );
}

// ---------------------------------------------------------------------------
// re-tenant and metadata-only mutations
// ---------------------------------------------------------------------------

#[tokio::test]
async fn move_to_org_carries_composite_fk_rows_and_revokes_shares() {
    let fixture = Fixture::new("move-org");
    fixture.create_org("other");
    let meta = fixture.publish_single("v1").await;
    let v2 = fixture
        .store
        .update_for(&meta, html_update(1, "v2"), mutation_audit())
        .await
        .expect("update")
        .meta;
    seed_engagement(&fixture, &meta.id);
    fixture.execute(&format!(
        "INSERT INTO org_discord_discussion_connections (id, org, url, label) \
           VALUES ('move-connection', '{TEST_ORG}', 'https://discord.invalid/api/webhooks/1/token', 'Move fixture');
         INSERT INTO artifact_discussions \
           (artifact_id, org, provider, mode, connection_org, connection_id, state, generation) \
           VALUES ('{id}', '{TEST_ORG}', 'discord', 'discord_mirror', '{TEST_ORG}', \
                   'move-connection', 'connected', 1);
         INSERT INTO provider_delivery_outbox \
           (id, provider, event_id, tenant, event_type, target_key, bucket_id, secret_ref, payload, \
            payload_sha256, state, next_attempt_at, created_at, updated_at, delivery_kind, ordering_key) \
           VALUES ('move-outbox', 'discord', 'move-event', '{TEST_ORG}', 'feedback', \
                   'move-connection', 'move-connection', 'webhook:move-connection', x'7b7d', \
                   '44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a', \
                   'accepted', 0, 0, 0, 'discussion_message', 'artifact:{id}');
         INSERT INTO discussion_message_links \
           (provider, artifact_id, org, connection_id, feedback_id, delivery_event_id, outbox_id, \
            external_thread_id, external_message_id, generation, state) \
           VALUES ('discord', '{id}', '{TEST_ORG}', 'move-connection', 'fb0000000000000a', \
                   'move-event', 'move-outbox', 'move-thread', 'move-message', 1, 'posted');",
        id = meta.id.0,
    ));

    let moved = fixture
        .store
        .move_to_org_for(&v2, "other", Some("moved"), mutation_audit())
        .await
        .expect("move succeeds");

    assert_eq!(moved.org.0, "other");
    assert_eq!(moved.category, "moved");
    assert_eq!(
        moved.client_id.0, TEST_CLIENT,
        "client_id stays so the old org-locked key cannot update it"
    );
    assert_eq!(
        fixture.scalar::<String>("SELECT org FROM feedback"),
        "other"
    );
    assert_eq!(
        fixture.scalar::<String>("SELECT org FROM artifact_revisions"),
        "other"
    );
    assert_eq!(
        fixture.scalar::<String>("SELECT org FROM artifact_views"),
        "other"
    );
    assert_eq!(
        fixture.count("SELECT COUNT(*) FROM artifact_shares"),
        0,
        "public shares are revoked, never carried into the new tenant"
    );
    assert_eq!(
        fixture.count("SELECT COUNT(*) FROM artifact_discussions"),
        0,
        "the source organization's discussion binding is revoked"
    );
    assert_eq!(
        fixture.count("SELECT COUNT(*) FROM discussion_message_links"),
        0,
        "source-org Discord message mappings are revoked"
    );
    assert_eq!(
        fixture.count("SELECT COUNT(*) FROM provider_delivery_outbox WHERE id='move-outbox'"),
        1,
        "immutable delivery history remains available"
    );
    assert_eq!(
        fixture.scalar::<String>(
            "SELECT tenant || ':' || operation || ':' || classification \
             FROM security_audit_events ORDER BY sequence DESC LIMIT 1",
        ),
        "acme:artifact.org.move:shares_revoked_1",
        "an admin-initiated cross-org move is filed under the affected source tenant without a share token"
    );
    fixture.execute(&format!(
        "INSERT INTO artifact_discussions \
           (artifact_id, org, provider, mode, state, generation) \
         VALUES ('{id}', 'other', 'discord', 'artifact_only', 'local', 0);",
        id = moved.id.0,
    ));
    let same_org = fixture
        .store
        .move_to_org_for(&moved, "other", Some("same org"), mutation_audit())
        .await
        .expect("same-org category update succeeds");
    assert_eq!(same_org.category, "same org");
    assert_eq!(
        fixture.count("SELECT COUNT(*) FROM artifact_discussions"),
        1,
        "same-org category update retains discussion state"
    );
    assert_eq!(
        fixture.foreign_key_violations(),
        0,
        "the deferred composite FKs hold at commit"
    );
}

#[tokio::test]
async fn move_to_org_rejects_an_unknown_organization() {
    let fixture = Fixture::new("move-unknown-org");
    let meta = fixture.publish_single("v1").await;
    assert_eq!(
        fixture
            .store
            .move_to_org_for(&meta, " nope ", None, mutation_audit())
            .await
            .expect_err("unknown org"),
        AppError::Validation("Unknown organization \"nope\".".to_owned())
    );
    assert_eq!(
        fixture.reload(&meta).map(|row| row.org.0),
        Some(TEST_ORG.to_owned())
    );
}

#[tokio::test]
async fn category_and_visibility_are_normalized_and_bump_updated_at() {
    let fixture = Fixture::new("metadata-writes");
    let meta = fixture.publish_single("v1").await;

    let categorized = fixture
        .store
        .set_category_for(&meta, "  Design   Docs  ", mutation_audit())
        .await
        .expect("set category");
    assert_eq!(categorized.category, "Design Docs");
    assert_eq!(categorized.revision, 1, "category is not a content change");

    let hidden = fixture
        .store
        .set_hidden_for(&categorized, true, mutation_audit())
        .await
        .expect("set hidden");
    assert!(hidden.hidden);
    assert_eq!(hidden.revision, 1);

    let listed = fixture
        .store
        .list_org_artifacts(&meta.org, false)
        .await
        .expect("list");
    assert!(listed.is_empty(), "hidden means unlisted");
    let listed_all = fixture
        .store
        .list_org_artifacts(&meta.org, true)
        .await
        .expect("list including hidden");
    assert_eq!(listed_all.len(), 1);
    let conn = fixture.pool.get().expect("checkout audit assertions");
    let mut statement = conn
        .prepare("SELECT operation,classification FROM security_audit_events ORDER BY sequence DESC LIMIT 2")
        .expect("prepare audit assertions");
    let rows = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .expect("query audit assertions")
        .collect::<Result<Vec<_>, _>>()
        .expect("read audit assertions");
    assert_eq!(
        rows,
        vec![
            ("artifact.visibility.set".to_owned(), "hidden".to_owned()),
            ("artifact.category.set".to_owned(), String::new()),
        ]
    );
}

// ---------------------------------------------------------------------------
// reads and listings
// ---------------------------------------------------------------------------

#[tokio::test]
async fn find_meta_conceals_reserved_ids() {
    let fixture = Fixture::new("find-meta");
    let meta = fixture.publish_single("v1").await;

    assert_eq!(
        fixture
            .store
            .find_meta(&meta.id)
            .await
            .expect("lookup")
            .map(|row| row.id),
        Some(meta.id.clone())
    );
    for reserved in ["mcp", "health", "settings", "raw", "s", "robots.txt"] {
        assert!(
            fixture
                .store
                .find_meta(&ArtifactId(reserved.to_owned()))
                .await
                .expect("lookup")
                .is_none(),
            "{reserved} can never address an artifact"
        );
    }
}

#[tokio::test]
async fn publisher_listings_are_org_scoped_unless_the_key_is_admin() {
    let fixture = Fixture::new("list-scoping");
    fixture.create_org("other");
    let meta = fixture.publish_single("v1").await;
    fixture
        .store
        .move_to_org_for(&meta, "other", None, mutation_audit())
        .await
        .expect("move");

    let scoped = fixture
        .store
        .list_for_publisher(&crate::u08_support::publisher())
        .await
        .expect("scoped list");
    assert!(
        scoped.is_empty(),
        "an org-locked key stops seeing an artifact moved to another tenant"
    );

    // Admin status is derived from `org == "admin"` (see `PublisherIdentity::is_admin`), so an
    // admin key is expressed by its org. This test previously set `is_admin: true` while leaving a
    // non-admin org — exactly the two-sources-of-truth case that motivated removing the field.
    let admin = artifact_mcp::model::PublisherIdentity {
        org: artifact_mcp::model::OrgId("admin".to_owned()),
        ..crate::u08_support::publisher()
    };
    let all = fixture
        .store
        .list_for_publisher(&admin)
        .await
        .expect("admin list");
    assert_eq!(all.len(), 1);
}

#[tokio::test]
async fn revision_bodies_are_readable_for_both_shapes() {
    let fixture = Fixture::new("revision-reads");
    let single = fixture.publish_single("v1").await;
    let single_v2 = fixture
        .store
        .update_for(&single, html_update(1, "v2"), mutation_audit())
        .await
        .expect("update")
        .meta;
    let body = fixture
        .store
        .read_revision_body_for(&single_v2, 1, None)
        .await
        .expect("read")
        .expect("revision 1 body");
    assert_eq!(body.content, b"v1");
    assert_eq!(body.content_type, "text/html; charset=utf-8");
    assert!(
        fixture
            .store
            .read_revision_body_for(&single_v2, 1, Some("index.html"))
            .await
            .expect("read")
            .is_none(),
        "a single-file revision has no bundle files"
    );

    let bundle = fixture
        .publish_bundle(&[("index.html", "one"), ("app.js", "1")], None)
        .await
        .meta;
    let bundle_v2 = fixture
        .store
        .update_for(
            &bundle,
            bundle_update(1, &[("index.html", "two")], None),
            mutation_audit(),
        )
        .await
        .expect("update")
        .meta;
    let entry = fixture
        .store
        .read_revision_body_for(&bundle_v2, 1, Some(""))
        .await
        .expect("read")
        .expect("revision 1 entry");
    assert_eq!(entry.content, b"one");
    let asset = fixture
        .store
        .read_revision_body_for(&bundle_v2, 1, Some("app.js"))
        .await
        .expect("read")
        .expect("revision 1 asset");
    assert_eq!(asset.content, b"1");
    assert_eq!(asset.content_type, "text/javascript; charset=utf-8");
}

// ---------------------------------------------------------------------------
// digest backfill
// ---------------------------------------------------------------------------

#[tokio::test]
async fn digest_backfill_is_idempotent_and_never_bumps_revision_or_timestamp() {
    let fixture = Fixture::new("backfill");
    let single = fixture.publish_single("legacy").await;
    let bundle = fixture
        .publish_bundle(&[("index.html", "legacy bundle")], None)
        .await
        .meta;

    // Rows created before the v17 migration carry the empty default.
    fixture.execute("UPDATE artifacts SET body_sha256 = ''");
    let before_updated_at = fixture.scalar::<String>(&format!(
        "SELECT updated_at FROM artifacts WHERE id = '{}'",
        single.id.0
    ));

    let report = fixture
        .store
        .backfill_body_digests()
        .await
        .expect("backfill runs");
    assert_eq!(report.scanned, 2);
    assert_eq!(report.updated, 2);
    assert_eq!(fixture.recorded_digest(&single), sha256_hex("legacy"));
    assert_eq!(
        fixture.recorded_digest(&bundle),
        bundle_manifest_digest(&[("index.html".to_owned(), "legacy bundle".to_owned())])
    );
    assert_eq!(
        fixture.reload(&single).map(|row| row.revision),
        Some(1),
        "backfill is metadata repair, not a content mutation"
    );
    assert_eq!(
        fixture.scalar::<String>(&format!(
            "SELECT updated_at FROM artifacts WHERE id = '{}'",
            single.id.0
        )),
        before_updated_at
    );

    let again = fixture
        .store
        .backfill_body_digests()
        .await
        .expect("second backfill");
    assert_eq!(again.scanned, 0);
    assert_eq!(again.updated, 0);
}

#[tokio::test]
async fn digest_backfill_skips_rows_whose_body_is_gone() {
    let fixture = Fixture::new("backfill-missing");
    let meta = fixture.publish_single("gone").await;
    fixture.execute("UPDATE artifacts SET body_sha256 = ''");
    std::fs::remove_file(fixture.artifact_dir.join(format!("{}.html", meta.id.0)))
        .expect("remove body");

    let report = fixture
        .store
        .backfill_body_digests()
        .await
        .expect("backfill runs");
    assert_eq!(report.scanned, 1);
    assert_eq!(report.updated, 0);
    assert_eq!(fixture.recorded_digest(&meta), "");
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// One row in every table that cascades off an artifact.
fn seed_engagement(fixture: &Fixture, id: &ArtifactId) {
    fixture.execute(&format!(
        "INSERT INTO reactions (email, artifact_id, favorite, vote) \
           VALUES ('viewer@example.com', '{id}', 1, 1);
         INSERT INTO feedback (id, artifact_id, org, viewer_email, body, artifact_revision) \
           VALUES ('fb0000000000000a', '{id}', '{TEST_ORG}', 'viewer@example.com', 'note', 1);
         INSERT INTO artifact_views (artifact_id, org, email) \
           VALUES ('{id}', '{TEST_ORG}', 'viewer@example.com');
         INSERT INTO artifact_shares (token, artifact_id, org, created_by) \
           VALUES ('share-token-0000000001', '{id}', '{TEST_ORG}', 'owner@example.com');",
        id = id.0,
    ));
}
