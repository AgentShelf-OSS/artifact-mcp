//! PBI-080 durable inbox, author isolation, replay, monotonic mutation, and loop prevention.

use artifact_mcp::{
    integrations::discord_inbound::{
        DELETION_TOMBSTONE, DiscordAuthor, DiscordMessage, IgnoreReason, InboundEvent,
        InboundEventKind, InboundResult, RejectReason,
    },
    model::{
        ArtifactId, ArtifactMeta, ClientId, EmailAddress, FeedbackId, OrgId, Timestamp, Viewer,
    },
    persistence::{
        db::{self, Database, DbPool},
        discord_inbound::DiscordInboundStore,
        feedback_delivery::{self, DeliveryPlanningContext},
    },
    security::audit::MutationAudit,
};

use crate::u03_support::{TempDataDir, foreign_key_violations, scalar};

const AUDIT_KEY: [u8; 32] = [0x80; 32];

fn admin_audit() -> MutationAudit {
    MutationAudit::viewer(&Viewer {
        email: Some(EmailAddress::from("admin@example.test")),
        org: Some(OrgId::from("admin")),
        is_admin: true,
    })
    .expect("admin audit")
}

async fn fixture(label: &str) -> (TempDataDir, DbPool, DiscordInboundStore) {
    let dir = TempDataDir::new(label);
    let pool = Database::open_at(dir.path()).expect("migrate");
    db::interact(&pool, |conn| {
        conn.execute("INSERT INTO orgs (name) VALUES ('acme')", [])
            .expect("org");
        conn.execute(
            "INSERT INTO artifacts (id, client_id, org, title) \
             VALUES ('artifact-a', 'publisher', 'acme', 'Artifact')",
            [],
        )
        .expect("artifact");
        conn.execute(
            "INSERT INTO org_discord_threading_policies (org, outbound_enabled) \
             VALUES ('acme', 1)",
            [],
        )
        .expect("outbound policy");
        // Insert as the legacy strategy to avoid fabricating publication evidence in this focused
        // inbound fixture, then make the already-bound destination notification-thread capable.
        conn.execute(
            "INSERT INTO org_webhooks (id, org, url, events) \
             VALUES ('webhook-a', 'acme', \
                     'https://discord.com/api/webhooks/123456789012345678/synthetic', \
                     'published')",
            [],
        )
        .expect("notification webhook");
        conn.execute(
            "INSERT INTO org_discord_discussion_connections \
             (id, org, url, label, strategy, notification_webhook_id, guild_id, channel_id) \
             VALUES ('connection-a', 'acme', '', 'Discord', 'forum_webhook', \
                     'webhook-a', '100', '200')",
            [],
        )
        .expect("connection");
        conn.execute(
            "INSERT INTO artifact_discussions \
             (artifact_id, org, provider, mode, connection_org, connection_id, thread_id, \
              root_message_id, state, generation) \
             VALUES ('artifact-a', 'acme', 'discord', 'discord_mirror', 'acme', \
                     'connection-a', '300', 'root', 'connected', 1)",
            [],
        )
        .expect("discussion");
        conn.execute(
            "UPDATE org_discord_discussion_connections \
                SET strategy='notification_thread', notification_provider_webhook_id='400' \
              WHERE id='connection-a'",
            [],
        )
        .expect("exact destination");
        conn.execute(
            "INSERT INTO discord_gateway_sessions \
             (org, credential_version, health) VALUES ('acme', 1, 'ready')",
            [],
        )
        .expect("gateway ready");
        Ok(())
    })
    .await
    .expect("seed");
    let store = DiscordInboundStore::new(pool.clone());
    store
        .set_policy_audited(
            ArtifactId::from("artifact-a"),
            OrgId::from("acme"),
            true,
            admin_audit(),
            AUDIT_KEY,
        )
        .await
        .expect("enable inbound");
    (dir, pool, store)
}

fn message(id: &str, body: Option<&str>, version: i64) -> DiscordMessage {
    DiscordMessage {
        id: id.to_owned(),
        guild_id: "100".to_owned(),
        thread_id: "300".to_owned(),
        author: DiscordAuthor {
            id: "500".to_owned(),
            display: "Discord Person".to_owned(),
            is_bot: false,
            webhook_id: None,
        },
        content: body.map(str::to_owned),
        reply_to_message_id: None,
        version,
        created_at: Some("2026-07-30T00:00:00Z".to_owned()),
        edited_at: (version > 1).then(|| "2026-07-30T00:01:00Z".to_owned()),
        supported_text: true,
    }
}

fn event(id: &str, kind: InboundEventKind, message: Option<DiscordMessage>) -> InboundEvent {
    InboundEvent {
        event_id: id.to_owned(),
        gateway_session_id: "session-a".to_owned(),
        org: OrgId::from("acme"),
        kind,
        message,
        guild_id: "100".to_owned(),
        thread_id: "300".to_owned(),
        payload_fingerprint: format!("{:064x}", id.bytes().map(u64::from).sum::<u64>()),
    }
}

#[tokio::test]
async fn create_replay_edit_delete_are_atomic_monotonic_and_never_echo_outbound() {
    let (_dir, pool, store) = fixture("u80-lifecycle").await;
    let create = event(
        "1",
        InboundEventKind::MessageCreate,
        Some(message("600", Some("hello from Discord"), 1)),
    );
    assert_eq!(
        store.apply_event(create.clone()).await.expect("create"),
        InboundResult::Applied
    );
    assert_eq!(
        store.apply_event(create).await.expect("replay"),
        InboundResult::Duplicate
    );
    {
        let conn = db::checkout(&pool).expect("checkout");
        assert_eq!(
            scalar::<i64>(
                &conn,
                "SELECT COUNT(*) FROM feedback WHERE id='discord:600' \
                 AND viewer_email IS NULL AND author_source='discord' \
                 AND external_author_id='500' AND external_author_display='Discord Person'"
            ),
            1
        );
        assert_eq!(
            scalar::<i64>(&conn, "SELECT COUNT(*) FROM discord_inbound_events"),
            1
        );
        assert_eq!(
            scalar::<i64>(&conn, "SELECT COUNT(*) FROM provider_delivery_outbox"),
            0
        );
    }

    assert_eq!(
        store
            .apply_event(event(
                "2",
                InboundEventKind::MessageUpdate,
                Some(message("600", Some("edited once"), 3)),
            ))
            .await
            .expect("edit"),
        InboundResult::Applied
    );
    assert_eq!(
        store
            .apply_event(event(
                "3",
                InboundEventKind::MessageUpdate,
                Some(message("600", Some("stale edit"), 2)),
            ))
            .await
            .expect("stale"),
        InboundResult::Ignored(IgnoreReason::Stale)
    );
    let mut deleted = message("600", None, 4);
    deleted.edited_at = Some("2026-07-30T00:02:00Z".to_owned());
    assert_eq!(
        store
            .apply_event(event("4", InboundEventKind::MessageDelete, Some(deleted),))
            .await
            .expect("delete"),
        InboundResult::Applied
    );
    let conn = db::checkout(&pool).expect("checkout");
    let body: String = conn
        .query_row(
            "SELECT body FROM feedback WHERE id='discord:600'",
            [],
            |row| row.get(0),
        )
        .expect("body");
    assert_eq!(body, DELETION_TOMBSTONE);
    assert_eq!(
        scalar::<i64>(&conn, "SELECT COUNT(*) FROM provider_delivery_outbox"),
        0
    );
}

#[tokio::test]
async fn partial_update_is_durable_and_remains_retryable_until_hydrated() {
    let (_dir, pool, store) = fixture("u80-partial").await;
    store
        .apply_event(event(
            "1",
            InboundEventKind::MessageCreate,
            Some(message("600", Some("original"), 1)),
        ))
        .await
        .expect("create");
    let partial = event(
        "2",
        InboundEventKind::MessageUpdate,
        Some(message("600", None, 2)),
    );
    assert_eq!(
        store.apply_event(partial.clone()).await.expect("defer"),
        InboundResult::NeedsFetch
    );
    assert_eq!(
        store
            .apply_event(partial.clone())
            .await
            .expect("retry remains open"),
        InboundResult::NeedsFetch
    );
    {
        let conn = db::checkout(&pool).expect("checkout");
        assert_eq!(
            scalar::<i64>(
                &conn,
                "SELECT COUNT(*) FROM discord_inbound_events \
                  WHERE event_id='2' AND result='needs_fetch' AND processed_at IS NULL"
            ),
            1
        );
    }

    let hydrated = event(
        "2",
        InboundEventKind::MessageUpdate,
        Some(message("600", Some("hydrated edit"), 2)),
    );
    assert_eq!(
        store.apply_event(hydrated).await.expect("hydrate"),
        InboundResult::Applied
    );
    assert!(
        store
            .pending_updates(&OrgId::from("acme"), 10)
            .await
            .expect("pending")
            .is_empty()
    );
    {
        let conn = db::checkout(&pool).expect("checkout");
        assert_eq!(
            scalar::<String>(&conn, "SELECT body FROM feedback WHERE id='discord:600'"),
            "hydrated edit"
        );
        conn.execute(
            "UPDATE discord_inbound_events SET processed_at='2000-01-01 00:00:00' \
              WHERE event_id='2'",
            [],
        )
        .expect("age terminal receipt");
    }
    assert_eq!(
        store
            .cleanup_processed_events(30, 1)
            .await
            .expect("cleanup"),
        1
    );
    let conn = db::checkout(&pool).expect("checkout");
    assert_eq!(
        scalar::<i64>(
            &conn,
            "SELECT COUNT(*) FROM discord_inbound_events WHERE event_id='2'"
        ),
        0
    );
}

#[tokio::test]
async fn resolving_discord_origin_feedback_never_enqueues_provider_delivery() {
    let (_dir, pool, store) = fixture("u80-resolve-loop").await;
    assert_eq!(
        store
            .apply_event(event(
                "1",
                InboundEventKind::MessageCreate,
                Some(message("600", Some("inbound"), 1)),
            ))
            .await
            .expect("create"),
        InboundResult::Applied
    );
    let planning = DeliveryPlanningContext::production();
    db::interact(&pool, move |conn| {
        conn.execute(
            "INSERT INTO org_webhooks (id, org, url, events) \
             VALUES ('resolved-target', 'acme', \
                     'https://discord.com/api/webhooks/987654321098765432/synthetic', \
                     'resolved')",
            [],
        )
        .expect("resolved subscriber");
        let meta = ArtifactMeta {
            id: ArtifactId::from("artifact-a"),
            client_id: ClientId::from("publisher"),
            org: OrgId::from("acme"),
            title: "Artifact".to_owned(),
            description: String::new(),
            bytes: 0,
            created_at: Timestamp(String::new()),
            updated_at: Timestamp(String::new()),
            uploader_label: String::new(),
            owner_email: None,
            is_bundle: false,
            entry: String::new(),
            revision: 1,
            category: String::new(),
            hidden: false,
            body_sha256: String::new(),
        };
        assert!(
            feedback_delivery::resolve_as_publisher(
                conn,
                &planning,
                "https://artifacts.test",
                &meta,
                FeedbackId::from("discord:600"),
                "agent:publisher",
            )
            .expect("resolve inbound")
        );
        assert_eq!(
            scalar::<i64>(conn, "SELECT COUNT(*) FROM provider_delivery_outbox"),
            0
        );
        Ok(())
    })
    .await
    .expect("resolve");
}

#[tokio::test]
async fn partial_update_backoff_is_durable_bounded_and_degrades_only_the_integration() {
    let (_dir, pool, store) = fixture("u80-backoff").await;
    store
        .apply_event(event(
            "1",
            InboundEventKind::MessageCreate,
            Some(message("600", Some("original"), 1)),
        ))
        .await
        .expect("create");
    let partial = event(
        "2",
        InboundEventKind::MessageUpdate,
        Some(message("600", None, 2)),
    );
    assert_eq!(
        store.apply_event(partial.clone()).await.expect("defer"),
        InboundResult::NeedsFetch
    );
    for attempt in 1..=20 {
        assert_eq!(
            store
                .defer_update_retry(&partial, 30, true)
                .await
                .expect("persist backoff"),
            attempt == 20
        );
    }
    assert!(
        store
            .pending_updates(&OrgId::from("acme"), 10)
            .await
            .expect("pending")
            .is_empty()
    );
    let conn = db::checkout(&pool).expect("checkout");
    assert_eq!(
        scalar::<i64>(
            &conn,
            "SELECT COUNT(*) FROM discord_inbound_events \
              WHERE event_id='2' AND result='failed' AND attempts=20 \
                AND safe_error='rate_limited' AND processed_at IS NOT NULL \
                AND next_attempt_at IS NULL"
        ),
        1
    );
    assert_eq!(
        scalar::<String>(
            &conn,
            "SELECT health FROM artifact_discord_inbound_policies \
              WHERE artifact_id='artifact-a' AND org='acme'"
        ),
        "degraded"
    );
    assert_eq!(
        scalar::<i64>(&conn, "SELECT COUNT(*) FROM provider_delivery_outbox"),
        0
    );
}

#[tokio::test]
async fn outbound_policy_alone_never_qualifies_an_organization_for_gateway_startup() {
    let (_dir, pool, store) = fixture("u80-explicit-gateway").await;
    assert_eq!(store.gateway_targets().await.expect("enabled").len(), 1);
    store
        .set_policy_audited(
            ArtifactId::from("artifact-a"),
            OrgId::from("acme"),
            false,
            admin_audit(),
            AUDIT_KEY,
        )
        .await
        .expect("disable inbound");
    assert!(
        store
            .gateway_targets()
            .await
            .expect("outbound only")
            .is_empty()
    );
    db::interact(&pool, |conn| {
        conn.execute(
            "UPDATE artifact_discord_inbound_policies \
                SET health='connecting' WHERE artifact_id='artifact-a' AND org='acme'",
            [],
        )
        .expect("stage readiness");
        Ok(())
    })
    .await
    .expect("stage");
    assert_eq!(
        store
            .gateway_targets()
            .await
            .expect("readiness target")
            .len(),
        1
    );
    store
        .set_gateway_health(OrgId::from("acme"), 1, "ready", "", None)
        .await
        .expect("ready");
    assert!(
        store
            .gateway_targets()
            .await
            .expect("readiness completed")
            .is_empty(),
        "a completed readiness probe must stop until two-way is explicitly enabled"
    );
    let metrics = store.operational_metrics().await.expect("metrics");
    assert_eq!(metrics.gateway_ready, 1);
    assert_eq!(metrics.inbox_depth, 0);
}

#[tokio::test]
async fn injected_mapping_failure_rolls_back_feedback_and_event_receipt_together() {
    let (_dir, pool, store) = fixture("u80-rollback").await;
    {
        let conn = db::checkout(&pool).expect("checkout");
        conn.execute_batch(
            "CREATE TRIGGER u80_fail_mapping BEFORE INSERT ON discord_inbound_message_state \
             BEGIN SELECT RAISE(ABORT, 'synthetic failure'); END;",
        )
        .expect("failpoint");
    }
    assert!(
        store
            .apply_event(event(
                "1",
                InboundEventKind::MessageCreate,
                Some(message("600", Some("must roll back"), 1)),
            ))
            .await
            .is_err()
    );
    let conn = db::checkout(&pool).expect("checkout");
    assert_eq!(scalar::<i64>(&conn, "SELECT COUNT(*) FROM feedback"), 0);
    assert_eq!(
        scalar::<i64>(&conn, "SELECT COUNT(*) FROM discord_inbound_events"),
        0
    );
}

#[tokio::test]
async fn cross_tenant_mapped_event_is_rejected_with_a_safe_durable_signal() {
    let (_dir, pool, store) = fixture("u80-cross-tenant").await;
    db::interact(&pool, |conn| {
        conn.execute("INSERT INTO orgs (name) VALUES ('other')", [])
            .expect("other org");
        Ok(())
    })
    .await
    .expect("other org");
    let mut foreign = event(
        "1",
        InboundEventKind::MessageCreate,
        Some(message("600", Some("foreign content"), 1)),
    );
    foreign.org = OrgId::from("other");
    assert_eq!(
        store.apply_event(foreign).await.expect("reject"),
        InboundResult::Rejected(RejectReason::CrossTenant)
    );
    let conn = db::checkout(&pool).expect("checkout");
    assert_eq!(scalar::<i64>(&conn, "SELECT COUNT(*) FROM feedback"), 0);
    assert_eq!(
        scalar::<i64>(
            &conn,
            "SELECT COUNT(*) FROM discord_inbound_events \
             WHERE org='other' AND result='rejected' AND safe_error='cross_tenant'"
        ),
        1
    );
}

#[tokio::test]
async fn unmapped_event_is_receipted_without_content_or_feedback() {
    let (_dir, pool, store) = fixture("u80-unmapped").await;
    let mut unmapped = event(
        "1",
        InboundEventKind::MessageCreate,
        Some(message("600", Some("unmapped content"), 1)),
    );
    unmapped.thread_id = "999".to_owned();
    if let Some(message) = unmapped.message.as_mut() {
        message.thread_id = "999".to_owned();
    }
    assert_eq!(
        store.apply_event(unmapped).await.expect("ignore"),
        InboundResult::Ignored(IgnoreReason::Unmapped)
    );
    let conn = db::checkout(&pool).expect("checkout");
    assert_eq!(scalar::<i64>(&conn, "SELECT COUNT(*) FROM feedback"), 0);
    assert_eq!(
        scalar::<i64>(
            &conn,
            "SELECT COUNT(*) FROM discord_inbound_events \
             WHERE org='acme' AND result='ignored' AND safe_error='unmapped'"
        ),
        1
    );
}

#[tokio::test]
async fn thread_loss_fails_the_discussion_and_gateway_health_cannot_erase_degradation() {
    let (_dir, pool, store) = fixture("u80-thread-loss").await;
    store
        .apply_event(event(
            "1",
            InboundEventKind::MessageCreate,
            Some(message("600", Some("canonical feedback"), 1)),
        ))
        .await
        .expect("create");
    assert_eq!(
        store
            .apply_event(event("2", InboundEventKind::ThreadDelete, None))
            .await
            .expect("thread delete"),
        InboundResult::Degraded(
            artifact_mcp::integrations::discord_inbound::ThreadDegradedReason::Deleted
        )
    );
    store
        .set_gateway_health(OrgId::from("acme"), 1, "ready", "", None)
        .await
        .expect("gateway remains healthy");

    let conn = db::checkout(&pool).expect("checkout");
    assert_eq!(
        scalar::<String>(
            &conn,
            "SELECT state FROM artifact_discussions WHERE artifact_id='artifact-a'"
        ),
        "failed"
    );
    assert_eq!(
        scalar::<String>(
            &conn,
            "SELECT last_error FROM artifact_discussions WHERE artifact_id='artifact-a'"
        ),
        "thread_unavailable"
    );
    assert_eq!(
        scalar::<String>(
            &conn,
            "SELECT health FROM artifact_discord_inbound_policies \
              WHERE artifact_id='artifact-a' AND org='acme'"
        ),
        "degraded"
    );
    assert_eq!(
        scalar::<String>(
            &conn,
            "SELECT safe_error FROM artifact_discord_inbound_policies \
              WHERE artifact_id='artifact-a' AND org='acme'"
        ),
        "thread_unavailable"
    );
    assert_eq!(
        scalar::<i64>(
            &conn,
            "SELECT COUNT(*) FROM feedback WHERE id='discord:600'"
        ),
        1,
        "canonical feedback survives provider thread loss"
    );
}

#[tokio::test]
async fn archived_or_locked_thread_uses_the_same_durable_failure_boundary() {
    for (label, archived, locked) in [
        ("u80-thread-archived", true, false),
        ("u80-thread-locked", false, true),
    ] {
        let (_dir, pool, store) = fixture(label).await;
        assert_eq!(
            store
                .apply_event(event(
                    "1",
                    InboundEventKind::ThreadUpdate { archived, locked },
                    None,
                ))
                .await
                .expect("thread state update"),
            InboundResult::Degraded(
                artifact_mcp::integrations::discord_inbound::ThreadDegradedReason::ArchivedOrLocked
            )
        );
        store
            .set_gateway_health(OrgId::from("acme"), 1, "ready", "", None)
            .await
            .expect("healthy organization Gateway");
        let conn = db::checkout(&pool).expect("checkout");
        assert_eq!(
            scalar::<String>(
                &conn,
                "SELECT state FROM artifact_discussions WHERE artifact_id='artifact-a'"
            ),
            "failed"
        );
        assert_eq!(
            scalar::<String>(
                &conn,
                "SELECT safe_error FROM artifact_discord_inbound_policies \
                  WHERE artifact_id='artifact-a' AND org='acme'"
            ),
            "thread_unavailable"
        );
    }
}

#[tokio::test]
async fn local_admin_deletion_leaves_a_provider_tombstone_instead_of_an_orphan_mapping() {
    let (_dir, pool, store) = fixture("u80-local-moderation").await;
    store
        .apply_event(event(
            "1",
            InboundEventKind::MessageCreate,
            Some(message("600", Some("moderate me"), 1)),
        ))
        .await
        .expect("create");
    {
        let conn = db::checkout(&pool).expect("checkout");
        conn.execute("DELETE FROM feedback WHERE id='discord:600'", [])
            .expect("admin delete");
        assert_eq!(
            scalar::<i64>(
                &conn,
                "SELECT COUNT(*) FROM discord_inbound_message_state \
                  WHERE external_message_id='600' AND feedback_id IS NULL"
            ),
            1,
            "the provider identity remains as a body-free local moderation tombstone"
        );
        assert_eq!(foreign_key_violations(&conn), 0);
    }

    assert_eq!(
        store
            .apply_event(event(
                "2",
                InboundEventKind::MessageUpdate,
                Some(message("600", Some("remote edit"), 2)),
            ))
            .await
            .expect("ignore update after moderation"),
        InboundResult::Ignored(IgnoreReason::UnknownMessage)
    );
    assert_eq!(
        store
            .apply_event(event(
                "3",
                InboundEventKind::MessageCreate,
                Some(message("600", Some("replayed create"), 3)),
            ))
            .await
            .expect("ignore replay after moderation"),
        InboundResult::Ignored(IgnoreReason::UnknownMessage)
    );
    let conn = db::checkout(&pool).expect("checkout");
    assert_eq!(scalar::<i64>(&conn, "SELECT COUNT(*) FROM feedback"), 0);
}

#[test]
fn frozen_v30_feedback_threads_anchors_and_resolution_upgrade_reopen_and_cascade() {
    let source = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("conformance/fixtures/historical/boundary-v30/artifacts.db");
    let source_bytes = std::fs::read(&source).expect("read frozen source");
    let dir = TempDataDir::new("u80-frozen-v30");
    let destination = db::database_path(dir.path());
    std::fs::copy(&source, &destination).expect("copy frozen source");
    {
        let conn = rusqlite::Connection::open(&destination).expect("open v30");
        conn.pragma_update(None, "foreign_keys", "ON")
            .expect("foreign keys");
        conn.execute_batch(
            "INSERT INTO feedback \
               (id, artifact_id, org, viewer_email, body, artifact_revision) VALUES \
               ('u80-root', 'singleb30', 'fixture', 'root@example.test', 'root', 1);
             INSERT INTO feedback \
               (id, artifact_id, org, viewer_email, body, artifact_revision, parent_id) VALUES \
               ('u80-reply', 'singleb30', 'fixture', 'reply@example.test', 'reply', 1, 'u80-root');
             INSERT INTO feedback \
               (id, artifact_id, org, viewer_email, body, artifact_revision, anchor_path, \
                anchor_x, anchor_y, anchor_w, anchor_h, anchor_approx) VALUES \
               ('u80-anchor', 'singleb30', 'fixture', 'anchor@example.test', 'anchor', 1, \
                '#review', 0.2, 0.3, 0.4, 0.2, 1);
             INSERT INTO feedback \
               (id, artifact_id, org, viewer_email, body, artifact_revision, resolved_at, resolved_by) VALUES \
               ('u80-resolved', 'singleb30', 'fixture', 'resolved@example.test', 'resolved', 1, \
                '2026-07-30 00:00:00', 'publisher');",
        )
        .expect("seed rich v30 rows");
    }

    let pool = Database::open_at(dir.path()).expect("upgrade v30");
    {
        let conn = db::checkout(&pool).expect("checkout");
        assert_eq!(
            scalar::<i64>(&conn, "SELECT MAX(version) FROM schema_migrations"),
            31
        );
        assert_eq!(
            scalar::<i64>(
                &conn,
                "SELECT COUNT(*) FROM feedback \
                 WHERE id LIKE 'u80-%' AND author_source='artifact' \
                   AND viewer_email IS NOT NULL AND external_author_id IS NULL"
            ),
            4
        );
        assert_eq!(
            scalar::<String>(&conn, "SELECT parent_id FROM feedback WHERE id='u80-reply'"),
            "u80-root"
        );
        assert_eq!(
            scalar::<i64>(
                &conn,
                "SELECT anchor_approx FROM feedback WHERE id='u80-anchor' \
                 AND anchor_x=0.2 AND anchor_y=0.3 AND anchor_w=0.4 AND anchor_h=0.2"
            ),
            1
        );
        assert_eq!(
            scalar::<String>(
                &conn,
                "SELECT resolved_by FROM feedback WHERE id='u80-resolved' \
                 AND resolved_at IS NOT NULL"
            ),
            "publisher"
        );
        conn.execute("DELETE FROM feedback WHERE id='u80-root'", [])
            .expect("delete root");
        assert_eq!(
            scalar::<i64>(
                &conn,
                "SELECT COUNT(*) FROM feedback WHERE id IN ('u80-root','u80-reply')"
            ),
            0
        );
    }
    drop(pool);
    let reopened = Database::open_at(dir.path()).expect("reopen v31");
    let conn = db::checkout(&reopened).expect("checkout reopened");
    assert_eq!(scalar::<String>(&conn, "PRAGMA integrity_check"), "ok");
    assert_eq!(foreign_key_violations(&conn), 0);
    assert_eq!(
        std::fs::read(source).expect("source remains frozen"),
        source_bytes
    );
}

#[test]
fn corrupted_cross_tenant_v30_feedback_fails_upgrade_and_rolls_back_migration() {
    let source = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("conformance/fixtures/historical/boundary-v30/artifacts.db");
    let dir = TempDataDir::new("u80-corrupt-v30");
    let destination = db::database_path(dir.path());
    std::fs::copy(source, &destination).expect("copy frozen source");
    {
        let conn = rusqlite::Connection::open(&destination).expect("open v30");
        conn.pragma_update(None, "foreign_keys", "OFF")
            .expect("disable foreign keys");
        conn.execute("INSERT INTO orgs (name) VALUES ('other')", [])
            .expect("other org");
        conn.execute(
            "INSERT INTO feedback \
             (id, artifact_id, org, viewer_email, body, artifact_revision) \
             VALUES ('u80-corrupt', 'singleb30', 'other', 'foreign@example.test', 'bad', 1)",
            [],
        )
        .expect("seed corruption");
    }
    assert!(
        Database::open_at(dir.path()).is_err(),
        "deferred v31 tenant constraints must reject corrupted v30 input"
    );
    let conn = rusqlite::Connection::open(destination).expect("reopen failed upgrade");
    assert_eq!(
        scalar::<i64>(&conn, "SELECT MAX(version) FROM schema_migrations"),
        30
    );
    assert_eq!(
        scalar::<i64>(
            &conn,
            "SELECT COUNT(*) FROM feedback WHERE id='u80-corrupt'"
        ),
        1,
        "failed migration must leave the source schema and evidence intact"
    );
}

#[tokio::test]
async fn inbound_message_mapping_trigger_rejects_cross_tenant_feedback_correlation() {
    let (_dir, pool, _store) = fixture("u80-tenant-trigger").await;
    db::interact(&pool, |conn| {
        conn.execute("INSERT INTO orgs (name) VALUES ('other')", [])
            .expect("other org");
        conn.execute(
            "INSERT INTO artifacts (id, client_id, org, title) \
             VALUES ('artifact-other', 'publisher', 'other', 'Other')",
            [],
        )
        .expect("other artifact");
        conn.execute(
            "INSERT INTO feedback \
             (id, artifact_id, org, viewer_email, body, artifact_revision, author_source, \
              external_author_id, external_author_display) \
             VALUES ('discord:foreign', 'artifact-other', 'other', NULL, 'foreign', 1, \
                     'discord', '700', 'Foreign')",
            [],
        )
        .expect("other feedback");
        let result = conn.execute(
            "INSERT INTO discord_inbound_message_state \
             (provider, external_message_id, org, artifact_id, feedback_id, \
              external_thread_id, external_author_id, external_author_display) \
             VALUES ('discord', '700', 'acme', 'artifact-a', 'discord:foreign', \
                     '300', '700', 'Foreign')",
            [],
        );
        assert!(result.is_err());
        Ok(())
    })
    .await
    .expect("tenant check");
}
