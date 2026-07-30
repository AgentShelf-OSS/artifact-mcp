//! PBI-079 DB1: optional Discord discussion persistence, secrets, and durable correlations.

use std::sync::Arc;

use artifact_mcp::{
    error::AppError,
    integrations::delivery_envelope::stable_delivery_event_id,
    model::{ArtifactId, EmailAddress, FeedbackId, OrgId, Viewer, WebhookEvent},
    persistence::{
        db::{self, Database, DbPool},
        discussions::{
            AcceptedDiscussionDelivery, AcceptedDiscussionMarker, BindDiscussionTombstone,
            CreateDiscussionConnection, CreateDiscussionMessageLink,
            CreateNotificationThreadConnection, DiscussionMode, DiscussionState, DiscussionStore,
            TerminalDiscussionDelivery,
        },
    },
    security::{audit::MutationAudit, crypto::WebhookUrlProtection},
};

use crate::u03_support::{TempDataDir, foreign_key_violations, scalar};

fn store(label: &str) -> (TempDataDir, DbPool, DiscussionStore) {
    let dir = TempDataDir::new(label);
    let pool = Database::open_at(dir.path()).expect("migrate database");
    let store = DiscussionStore::new(pool.clone(), Arc::new(WebhookUrlProtection::Plaintext));
    (dir, pool, store)
}

const AUDIT_KEY: [u8; 32] = [0x79; 32];

fn admin_audit() -> MutationAudit {
    MutationAudit::viewer(&Viewer {
        email: Some(EmailAddress("admin@example.test".to_owned())),
        org: Some(OrgId("admin".to_owned())),
        is_admin: true,
    })
    .expect("admin audit")
}

async fn seed(pool: &DbPool, org: &str, artifact: &str, feedback: Option<&str>) {
    let org = org.to_owned();
    let artifact = artifact.to_owned();
    let feedback = feedback.map(str::to_owned);
    db::interact(pool, move |conn| {
        conn.execute("INSERT INTO orgs (name) VALUES (?1)", [&org])
            .expect("org");
        conn.execute(
            "INSERT INTO artifacts (id, client_id, org, title) VALUES (?1, 'publisher', ?2, 'Artifact')",
            [&artifact, &org],
        )
        .expect("artifact");
        if let Some(feedback) = feedback {
            conn.execute(
                "INSERT INTO feedback (id, artifact_id, org, viewer_email, body, artifact_revision) VALUES (?1, ?2, ?3, 'viewer@example.test', 'comment', 1)",
                [&feedback, &artifact, &org],
            )
            .expect("feedback");
        }
        Ok(())
    })
    .await
    .expect("seed");
}

async fn outbox(pool: &DbPool, id: &str, event: &str, tenant: &str, target: &str) {
    let id = id.to_owned();
    let event = event.to_owned();
    let tenant = tenant.to_owned();
    let target = target.to_owned();
    db::interact(pool, move |conn| {
        conn.execute(
            "INSERT INTO provider_delivery_outbox (id, provider, event_id, tenant, event_type, target_key, bucket_id, secret_ref, payload, payload_sha256, state, next_attempt_at, created_at, updated_at) VALUES (?1, 'discord', ?2, ?3, 'discussion', ?4, ?4, 'discussion:connection', x'7B7D', '44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a', 'ready', 0, 0, 0)",
            [&id, &event, &tenant, &target],
        ).expect("outbox");
        Ok(())
    }).await.expect("outbox seed");
}

async fn discussion_outbox(pool: &DbPool, id: &str, event: &str, tenant: &str, connection: &str) {
    let id = id.to_owned();
    let event = event.to_owned();
    let tenant = tenant.to_owned();
    let connection = connection.to_owned();
    db::interact(pool, move |conn| {
        let secret = format!("discussion:{connection}");
        conn.execute(
            "INSERT INTO provider_delivery_outbox (id, provider, event_id, tenant, event_type, target_key, bucket_id, secret_ref, payload, payload_sha256, state, next_attempt_at, created_at, updated_at) VALUES (?1, 'discord', ?2, ?3, 'discussion', ?4, ?4, ?5, x'7B7D', '44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a', 'ready', 0, 0, 0)",
            [&id, &event, &tenant, &connection, &secret],
        )
        .expect("discussion outbox");
        Ok(())
    })
    .await
    .expect("discussion outbox seed");
}

async fn set_tombstone_contract(
    pool: &DbPool,
    tenant: &str,
    target: &str,
    secret: &str,
    dependency: Option<&str>,
) {
    let tenant = tenant.to_owned();
    let target = target.to_owned();
    let secret = secret.to_owned();
    let dependency = dependency.map(str::to_owned);
    db::interact(pool, move |conn| {
        conn.execute(
            "UPDATE provider_delivery_outbox SET tenant = ?1, target_key = ?2, bucket_id = ?2, secret_ref = ?3, depends_on_outbox_id = ?4 WHERE id = 'tombstone-a'",
            rusqlite::params![tenant, target, secret, dependency],
        )
        .expect("tombstone contract");
        Ok(())
    })
    .await
    .expect("tombstone contract update");
}

async fn lease_discussion_outbox(
    pool: &DbPool,
    id: &str,
    kind: &str,
    ordering_key: &str,
    token: &str,
) {
    let id = id.to_owned();
    let kind = kind.to_owned();
    let ordering_key = ordering_key.to_owned();
    let token = token.to_owned();
    db::interact(pool, move |conn| {
        conn.execute(
            "UPDATE provider_delivery_outbox SET delivery_kind = ?1, ordering_key = ?2, state = 'leased', attempts = 1, lease_owner = 'worker-a', lease_token = ?3, lease_expires_at = 1000, lease_version = 1 WHERE id = ?4",
            [&kind, &ordering_key, &token, &id],
        )
        .expect("lease");
        Ok(())
    })
    .await
    .expect("lease");
}

#[tokio::test]
async fn marker_acceptance_and_terminal_are_atomic_and_link_free() {
    let (_dir, pool, store) = store("u79-marker-worker");
    seed(&pool, "acme", "artifact-a", Some("feedback-a")).await;
    let connection = store
        .upsert_connection(CreateDiscussionConnection {
            org: "acme".into(),
            url: "https://discord.com/api/webhooks/123456/secret".into(),
            label: "Forum".into(),
        })
        .await
        .expect("connection");
    let discussion = store
        .enable_mirror(
            &ArtifactId::from("artifact-a"),
            &OrgId::from("acme"),
            "owner",
        )
        .await
        .expect("mirror");
    let ordering = "discussion:artifact-a:1";
    discussion_outbox(&pool, "marker-a", "marker-event-a", "acme", &connection.id).await;
    lease_discussion_outbox(&pool, "marker-a", "discussion_message", ordering, "lease-a").await;
    db::interact(&pool, move |conn| {
        conn.execute(
            "UPDATE artifact_discussions SET thread_id = '234567890123456789' WHERE artifact_id = 'artifact-a'",
            [],
        )
        .expect("thread");
        Ok(())
    })
    .await
    .expect("thread update");
    assert!(
        store
            .accept_marker_delivery(AcceptedDiscussionMarker {
                outbox_id: "marker-a".into(),
                worker: "worker-a".into(),
                lease_token: "lease-a".into(),
                lease_version: 1,
                artifact_id: "artifact-a".into(),
                org: "acme".into(),
                connection_id: connection.id.clone(),
                generation: discussion.generation,
                message_id: "345678901234567890".into(),
                now_millis: 10,
            })
            .await
            .expect("accept marker")
    );
    {
        let conn = db::checkout(&pool).expect("checkout");
        assert_eq!(
            scalar::<String>(
                &conn,
                "SELECT state FROM provider_delivery_outbox WHERE id = 'marker-a'"
            ),
            "accepted"
        );
        assert_eq!(
            scalar::<i64>(&conn, "SELECT COUNT(*) FROM discussion_message_links"),
            0
        );
    }

    discussion_outbox(&pool, "marker-b", "marker-event-b", "acme", &connection.id).await;
    lease_discussion_outbox(&pool, "marker-b", "discussion_message", ordering, "lease-b").await;
    assert!(
        store
            .terminal_delivery(TerminalDiscussionDelivery {
                outbox_id: "marker-b".into(),
                worker: "worker-a".into(),
                lease_token: "lease-b".into(),
                lease_version: 1,
                artifact_id: "artifact-a".into(),
                org: "acme".into(),
                connection_id: connection.id,
                generation: discussion.generation,
                delivery_kind: "discussion_message".into(),
                feedback_id: None,
                classification: "validation_failed".into(),
                duplicate_risk: false,
                now_millis: 11,
            })
            .await
            .expect("terminal marker")
    );
    let conn = db::checkout(&pool).expect("checkout");
    assert_eq!(
        scalar::<String>(
            &conn,
            "SELECT state FROM provider_delivery_outbox WHERE id = 'marker-b'"
        ),
        "dead_letter"
    );
    assert_eq!(
        scalar::<i64>(&conn, "SELECT COUNT(*) FROM discussion_message_links"),
        0
    );
}

#[tokio::test]
async fn terminal_tombstone_dead_letters_and_retains_external_message_evidence() {
    let (_dir, pool, store) = store("u79-terminal-tombstone");
    seed(&pool, "acme", "artifact-a", Some("feedback-a")).await;
    let connection = store
        .upsert_connection(CreateDiscussionConnection {
            org: "acme".into(),
            url: "https://discord.com/api/webhooks/123456/secret".into(),
            label: "Forum".into(),
        })
        .await
        .expect("connection");
    let discussion = store
        .enable_mirror(
            &ArtifactId::from("artifact-a"),
            &OrgId::from("acme"),
            "owner",
        )
        .await
        .expect("mirror");
    discussion_outbox(&pool, "prior-post", "prior-event", "acme", &connection.id).await;
    discussion_outbox(
        &pool,
        "terminal-tombstone",
        "tombstone-event",
        "acme",
        &connection.id,
    )
    .await;
    lease_discussion_outbox(
        &pool,
        "terminal-tombstone",
        "discussion_tombstone",
        "discussion:artifact-a:1",
        "tombstone-lease",
    )
    .await;
    let connection_id = connection.id.clone();
    db::interact(&pool, move |conn| {
        conn.execute(
            "UPDATE provider_delivery_outbox SET state = 'accepted' WHERE id = 'prior-post'",
            [],
        )
        .expect("accepted post");
        conn.execute(
            "INSERT INTO discussion_message_links \
             (provider, artifact_id, org, connection_id, feedback_id, delivery_event_id, outbox_id, tombstone_outbox_id, external_thread_id, external_message_id, generation, state, local_deleted_at) \
             VALUES ('discord', 'artifact-a', 'acme', ?1, 'feedback-a', 'prior-event', 'prior-post', 'terminal-tombstone', '123456789012345678', '223456789012345678', 1, 'tombstone_pending', datetime('now'))",
            [connection_id],
        )
        .expect("retained tombstone mapping");
        Ok(())
    })
    .await
    .expect("seed retained mapping");

    assert!(
        store
            .terminal_delivery(TerminalDiscussionDelivery {
                outbox_id: "terminal-tombstone".into(),
                worker: "worker-a".into(),
                lease_token: "tombstone-lease".into(),
                lease_version: 1,
                artifact_id: "artifact-a".into(),
                org: "acme".into(),
                connection_id: connection.id,
                generation: discussion.generation,
                delivery_kind: "discussion_tombstone".into(),
                feedback_id: Some("feedback-a".into()),
                classification: "forbidden".into(),
                duplicate_risk: false,
                now_millis: 12,
            })
            .await
            .expect("terminal tombstone")
    );
    let conn = db::checkout(&pool).expect("checkout");
    assert_eq!(
        scalar::<String>(
            &conn,
            "SELECT state FROM provider_delivery_outbox WHERE id = 'terminal-tombstone'"
        ),
        "dead_letter"
    );
    assert_eq!(
        scalar::<String>(
            &conn,
            "SELECT state FROM discussion_message_links WHERE tombstone_outbox_id = 'terminal-tombstone'"
        ),
        "failed"
    );
    assert_eq!(
        scalar::<String>(
            &conn,
            "SELECT external_message_id FROM discussion_message_links WHERE tombstone_outbox_id = 'terminal-tombstone'"
        ),
        "223456789012345678"
    );
}

#[tokio::test]
async fn audited_configuration_and_mode_changes_are_atomic_idempotent_and_redacted() {
    let (_dir, pool, store) = store("u79-audited-settings");
    seed(&pool, "acme", "artifact-a", None).await;
    let secret = "https://discord.com/api/webhooks/123456/secret-must-not-persist-in-audit";
    let connection = store
        .upsert_connection_audited(
            CreateDiscussionConnection {
                org: "acme".into(),
                url: secret.into(),
                label: "Forum".into(),
            },
            admin_audit(),
            AUDIT_KEY,
        )
        .await
        .expect("audited configuration");
    assert!(connection.destination.contains("…"));

    let first = store
        .set_mode_audited(
            ArtifactId::from("artifact-a"),
            OrgId::from("acme"),
            DiscussionMode::DiscordMirror,
            "owner@example.test".into(),
            admin_audit(),
            AUDIT_KEY,
        )
        .await
        .expect("enable mirror");
    let repeated = store
        .set_mode_audited(
            ArtifactId::from("artifact-a"),
            OrgId::from("acme"),
            DiscussionMode::DiscordMirror,
            "owner@example.test".into(),
            admin_audit(),
            AUDIT_KEY,
        )
        .await
        .expect("idempotent enable");
    assert_eq!(first.generation, 1);
    assert_eq!(repeated.generation, first.generation);

    {
        let conn = db::checkout(&pool).expect("checkout");
        let audit_count = scalar::<i64>(&conn, "SELECT COUNT(*) FROM security_audit_events");
        assert_eq!(
            audit_count, 2,
            "the repeated desired state must not be audited"
        );
        let canonical: String = scalar(
            &conn,
            "SELECT group_concat(canonical, '\n') FROM security_audit_events",
        );
        assert!(!canonical.contains(secret));
        assert!(!canonical.contains(&connection.id));
    }

    let error = store
        .remove_connection_audited(OrgId::from("acme"), admin_audit(), AUDIT_KEY)
        .await
        .expect_err("bound credential cannot be removed");
    assert!(matches!(error, AppError::Conflict(_)));
}

#[tokio::test]
async fn notification_anchor_resolution_requires_the_exact_artifact_publication_event() {
    let (_dir, pool, store) = store("u79-notification-anchor-binding");
    seed(&pool, "acme", "artifact-a", None).await;
    let expected_event_id = stable_delivery_event_id(
        &OrgId::from("acme"),
        &WebhookEvent::Published,
        "artifact:artifact-a:1",
    );
    db::interact(&pool, move |conn| {
        conn.execute(
            "INSERT INTO org_webhooks (id, org, url, label, events) \
             VALUES ('webhook-a', 'acme', 'https://discord.com/api/webhooks/123456/secret', \
                     'Artifacts', 'published')",
            [],
        )
        .expect("webhook");
        conn.execute(
            "INSERT INTO provider_delivery_outbox \
             (id, provider, event_id, tenant, event_type, target_key, bucket_id, secret_ref, \
              payload, payload_sha256, state, next_attempt_at, discord_message_id, \
              created_at, updated_at, completed_at) \
             VALUES ('publication-anchor', 'discord', ?1, 'acme', 'published', 'webhook-a', \
                     'webhook-a', 'webhook:webhook-a', x'7B7D', \
                     '44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a', \
                     'accepted', 0, '223456789012345678', 0, 0, 0)",
            [expected_event_id],
        )
        .expect("publication outbox");
        Ok(())
    })
    .await
    .expect("seed notification anchor");

    let connection = store
        .upsert_notification_thread_connection_audited(
            CreateNotificationThreadConnection {
                org: OrgId::from("acme"),
                notification_webhook_id: "webhook-a".to_owned(),
                notification_provider_webhook_id: "523456789012345678".to_owned(),
                channel_id: "123456789012345678".to_owned(),
                guild_id: "323456789012345678".to_owned(),
                label: "Artifact threads".to_owned(),
            },
            admin_audit(),
            AUDIT_KEY,
        )
        .await
        .expect("notification thread connection");
    let discussion = store
        .set_mode_audited(
            ArtifactId::from("artifact-a"),
            OrgId::from("acme"),
            DiscussionMode::DiscordMirror,
            "owner@example.test".to_owned(),
            admin_audit(),
            AUDIT_KEY,
        )
        .await
        .expect("enable notification mirror");

    assert_eq!(
        store
            .notification_anchor_message(
                "publication-anchor",
                &ArtifactId::from("artifact-a"),
                &OrgId::from("acme"),
                &connection.id,
                discussion.generation,
            )
            .await
            .expect("resolve exact anchor")
            .as_deref(),
        Some("223456789012345678")
    );

    db::interact(&pool, move |conn| {
        conn.execute(
            "UPDATE provider_delivery_outbox SET event_id = 'wrong-publication' \
             WHERE id = 'publication-anchor'",
            [],
        )
        .expect("tamper publication identity");
        Ok(())
    })
    .await
    .expect("tamper anchor");
    assert!(
        store
            .notification_anchor_message(
                "publication-anchor",
                &ArtifactId::from("artifact-a"),
                &OrgId::from("acme"),
                &connection.id,
                discussion.generation,
            )
            .await
            .expect("tampered anchor is concealed")
            .is_none()
    );
}

#[tokio::test]
async fn recovered_notification_anchor_requires_every_exact_destination_dimension() {
    let (_dir, pool, store) = store("u81-recovered-notification-anchor-binding");
    seed(&pool, "acme", "artifact-a", None).await;
    db::interact(&pool, move |conn| {
        conn.execute(
            "INSERT INTO org_webhooks (id, org, url, label, events) \
             VALUES ('webhook-a', 'acme', 'https://discord.com/api/webhooks/123456/secret', \
                     'Artifacts', 'published')",
            [],
        )
        .expect("webhook");
        Ok(())
    })
    .await
    .expect("seed webhook");
    let connection = store
        .upsert_notification_thread_connection_audited(
            CreateNotificationThreadConnection {
                org: OrgId::from("acme"),
                notification_webhook_id: "webhook-a".to_owned(),
                notification_provider_webhook_id: "523456789012345678".to_owned(),
                channel_id: "123456789012345678".to_owned(),
                guild_id: "323456789012345678".to_owned(),
                label: "Artifact threads".to_owned(),
            },
            admin_audit(),
            AUDIT_KEY,
        )
        .await
        .expect("notification thread connection");
    let connection_id = connection.id.clone();
    db::interact(&pool, move |conn| {
        conn.execute(
            "INSERT INTO discord_notification_anchor_recoveries \
             (artifact_id, org, connection_id, notification_webhook_id, provider_webhook_id, \
              guild_id, channel_id, canonical_artifact_url, state, recovered_message_id, provenance) \
             VALUES ('artifact-a', 'acme', ?1, 'webhook-a', '523456789012345678', \
                     '323456789012345678', '123456789012345678', \
                     'https://artifacts.test/artifact-a', 'recovered', \
                     '623456789012345678', 'exact_selected_webhook_canonical_url')",
            [connection_id],
        )
        .expect("exact recovery");
        Ok(())
    })
    .await
    .expect("seed exact recovery");
    let discussion = store
        .set_mode_audited(
            ArtifactId::from("artifact-a"),
            OrgId::from("acme"),
            DiscussionMode::DiscordMirror,
            "owner@example.test".to_owned(),
            admin_audit(),
            AUDIT_KEY,
        )
        .await
        .expect("enable from recovery evidence");
    assert!(discussion.anchor_outbox_id.is_none());
    assert_eq!(
        store
            .notification_anchor_message(
                "",
                &ArtifactId::from("artifact-a"),
                &OrgId::from("acme"),
                &connection.id,
                discussion.generation,
            )
            .await
            .expect("recovered anchor")
            .as_deref(),
        Some("623456789012345678")
    );

    let connection_id = connection.id.clone();
    db::interact(&pool, move |conn| {
        conn.execute(
            "UPDATE org_discord_discussion_connections \
             SET notification_provider_webhook_id='723456789012345678' WHERE id=?1",
            [connection_id],
        )
        .expect("replace destination identity");
        Ok(())
    })
    .await
    .expect("tamper destination binding");
    assert!(
        store
            .notification_anchor_message(
                "",
                &ArtifactId::from("artifact-a"),
                &OrgId::from("acme"),
                &connection.id,
                discussion.generation,
            )
            .await
            .expect("mismatched recovery concealed")
            .is_none()
    );
}

#[tokio::test]
async fn connection_test_audit_is_bound_to_the_exact_immutable_connection() {
    let (_dir, pool, store) = store("u79-test-audit-binding");
    seed(&pool, "acme", "artifact-a", None).await;
    let connection = store
        .upsert_connection(CreateDiscussionConnection {
            org: "acme".into(),
            url: "https://discord.com/api/webhooks/123456/secret".into(),
            label: "Forum".into(),
        })
        .await
        .expect("connection");

    store
        .audit_connection_test(
            OrgId::from("acme"),
            connection.id.clone(),
            None,
            admin_audit(),
            AUDIT_KEY,
        )
        .await
        .expect("requested audit");
    store
        .audit_connection_test(
            OrgId::from("acme"),
            connection.id.clone(),
            Some(true),
            admin_audit(),
            AUDIT_KEY,
        )
        .await
        .expect("completed audit");
    {
        let conn = db::checkout(&pool).expect("checkout");
        let targets: Vec<(String, String)> = conn
            .prepare("SELECT target_type, target_id FROM security_audit_events ORDER BY sequence")
            .expect("query")
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .expect("rows")
            .map(Result::unwrap)
            .collect();
        assert_eq!(
            targets,
            vec![
                ("discussion_connection".to_owned(), connection.id.clone()),
                ("discussion_connection".to_owned(), connection.id.clone()),
            ]
        );
    }

    // A replacement/removal race records the terminal failure against the original immutable
    // identity, but cannot update a newly configured credential or claim detached success.
    assert!(
        store
            .remove_connection(&OrgId::from("acme"))
            .await
            .expect("remove")
    );
    let error = store
        .audit_connection_test(
            OrgId::from("acme"),
            connection.id.clone(),
            Some(true),
            admin_audit(),
            AUDIT_KEY,
        )
        .await
        .expect_err("detached completion");
    assert!(matches!(error, AppError::Conflict(_)));
    let conn = db::checkout(&pool).expect("checkout after detached completion");
    let terminal: (String, String, String, String) = conn
        .query_row(
            "SELECT target_type, target_id, result, classification FROM security_audit_events ORDER BY sequence DESC LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .expect("terminal failure audit");
    assert_eq!(
        terminal,
        (
            "discussion_connection".to_owned(),
            connection.id,
            "failure".to_owned(),
            "external_delivery_failed".to_owned(),
        )
    );
    assert_eq!(
        scalar::<i64>(
            &conn,
            "SELECT COUNT(*) FROM org_discord_discussion_connections"
        ),
        0,
        "a detached completion must not update or recreate a current connection"
    );
}

#[tokio::test]
async fn migration_preserves_local_only_default_and_legacy_outbox_ordering() {
    let (_dir, pool, _store) = store("u79-default");
    seed(&pool, "acme", "artifact-a", None).await;
    outbox(&pool, "outbox-a", "event-a", "acme", "connection-a").await;
    let conn = db::checkout(&pool).expect("checkout");
    assert_eq!(
        scalar::<i64>(&conn, "SELECT COUNT(*) FROM artifact_discussions"),
        0
    );
    assert_eq!(
        scalar::<String>(
            &conn,
            "SELECT ordering_key FROM provider_delivery_outbox WHERE id = 'outbox-a'"
        ),
        "connection-a"
    );
    for table in [
        "org_discord_discussion_connections",
        "artifact_discussions",
        "discussion_message_links",
    ] {
        assert_eq!(
            scalar::<i64>(
                &conn,
                &format!(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = '{table}'"
                )
            ),
            1,
            "{table}"
        );
    }
    assert_eq!(foreign_key_violations(&conn), 0);
}

#[tokio::test]
async fn connection_is_redacted_immutable_while_active_and_tenant_bound() {
    let (_dir, pool, store) = store("u79-connection");
    seed(&pool, "acme", "artifact-a", None).await;
    let summary = store
        .upsert_connection(CreateDiscussionConnection {
            org: "acme".into(),
            url: "https://discord.com/api/webhooks/123456/very-secret-token".into(),
            label: "Forum".into(),
        })
        .await
        .expect("connection");
    assert!(!summary.destination.contains("very-secret-token"));
    let old_connection_id = summary.id;
    let replacement = store
        .upsert_connection(CreateDiscussionConnection {
            org: "acme".into(),
            url: "https://discord.com/api/webhooks/987654/replacement-token".into(),
            label: "Replacement".into(),
        })
        .await
        .expect("safe unbound replacement");
    assert_ne!(replacement.id, old_connection_id);
    assert!(
        store
            .connection_for_delivery(&old_connection_id, &OrgId::from("acme"))
            .await
            .expect("old lookup")
            .is_none(),
        "the old immutable credential ID cannot resolve to a replacement URL"
    );
    let enabled = store
        .enable_mirror(
            &ArtifactId::from("artifact-a"),
            &OrgId::from("acme"),
            "owner@example.test",
        )
        .await
        .expect("enable");
    let delivery = store
        .connection_for_delivery(
            enabled.connection_id.as_deref().expect("connection id"),
            &OrgId::from("acme"),
        )
        .await
        .expect("delivery")
        .expect("configured");
    assert!(delivery.url.ends_with("replacement-token"));
    assert!(!format!("{delivery:?}").contains("replacement-token"));
    assert_eq!(enabled.state, DiscussionState::Pending);
    assert_eq!(enabled.generation, 1);
    assert!(
        store.remove_connection(&OrgId::from("acme")).await.is_err(),
        "active credential cannot be removed"
    );
    assert!(
        store
            .upsert_connection(CreateDiscussionConnection {
                org: "acme".into(),
                url: "https://discord.com/api/webhooks/777777/third-token".into(),
                label: "Third".into()
            })
            .await
            .is_err(),
        "active credential cannot be replaced"
    );
}

#[tokio::test]
async fn message_links_retain_local_deletion_and_reject_cross_tenant_rows() {
    let (_dir, pool, store) = store("u79-links");
    seed(&pool, "acme", "artifact-a", Some("feedback-a")).await;
    outbox(&pool, "outbox-a", "event-a", "acme", "connection-a").await;
    store
        .upsert_connection(CreateDiscussionConnection {
            org: "acme".into(),
            url: "https://discord.com/api/webhooks/123456/secret".into(),
            label: "Forum".into(),
        })
        .await
        .expect("connection");
    let connection_id: String = db::interact(&pool, move |conn| {
        conn.query_row(
            "SELECT id FROM org_discord_discussion_connections WHERE org = 'acme'",
            [],
            |row| row.get(0),
        )
        .map_err(|_| AppError::Internal)
    })
    .await
    .expect("connection id");
    let created = store
        .create_message_link(CreateDiscussionMessageLink {
            artifact_id: "artifact-a".into(),
            org: "acme".into(),
            connection_id: connection_id.clone(),
            feedback_id: "feedback-a".into(),
            delivery_event_id: "event-a".into(),
            outbox_id: "outbox-a".into(),
            external_thread_id: None,
            generation: 1,
        })
        .await
        .expect("link");
    assert_eq!(created.generation, 1);
    db::interact(&pool, move |conn| {
        conn.execute("DELETE FROM feedback WHERE id = 'feedback-a'", [])
            .expect("delete local feedback");
        Ok(())
    })
    .await
    .expect("delete");
    assert!(
        store
            .message_link_for_feedback(
                &ArtifactId::from("artifact-a"),
                &OrgId::from("acme"),
                &FeedbackId::from("feedback-a"),
            )
            .await
            .expect("link query")
            .is_some(),
        "mapping is evidence, not a cascading child"
    );
    assert!(
        store
            .mark_feedback_locally_deleted(
                &ArtifactId::from("artifact-a"),
                &OrgId::from("acme"),
                &FeedbackId::from("feedback-a"),
            )
            .await
            .expect("mark deletion")
    );
    let retained = store
        .message_link_for_feedback(
            &ArtifactId::from("artifact-a"),
            &OrgId::from("acme"),
            &FeedbackId::from("feedback-a"),
        )
        .await
        .expect("retained")
        .expect("mapping");
    assert_eq!(retained.state, "local_deleted");
    assert!(retained.local_deleted_at.is_some());
    outbox(&pool, "outbox-b", "event-b", "acme", "connection-b").await;
    let conn = db::checkout(&pool).expect("checkout");
    let error = conn.execute("INSERT INTO discussion_message_links (provider, artifact_id, org, connection_id, feedback_id, delivery_event_id, outbox_id, generation, state) VALUES ('discord', 'artifact-a', 'acme', ?1, 'foreign-feedback', 'bad', 'outbox-b', 2, 'pending')", [&connection_id]).expect_err("tenant trigger");
    assert!(
        error
            .to_string()
            .contains("discussion message link must match feedback artifact")
    );
}

#[tokio::test]
async fn accepted_delivery_after_local_delete_promotes_mapping_to_tombstone_pending() {
    let (_dir, pool, store) = store("u79-accepted");
    seed(&pool, "acme", "artifact-a", Some("feedback-a")).await;
    store
        .upsert_connection(CreateDiscussionConnection {
            org: "acme".into(),
            url: "https://discord.com/api/webhooks/123456/secret".into(),
            label: "Forum".into(),
        })
        .await
        .expect("connection");
    let discussion = store
        .enable_mirror(
            &ArtifactId::from("artifact-a"),
            &OrgId::from("acme"),
            "owner@example.test",
        )
        .await
        .expect("enable");
    outbox(&pool, "outbox-a", "event-a", "acme", "connection-a").await;
    store
        .create_message_link(CreateDiscussionMessageLink {
            artifact_id: "artifact-a".into(),
            org: "acme".into(),
            connection_id: discussion.connection_id.expect("connection id"),
            feedback_id: "feedback-a".into(),
            delivery_event_id: "event-a".into(),
            outbox_id: "outbox-a".into(),
            external_thread_id: None,
            generation: 1,
        })
        .await
        .expect("link");
    assert!(
        store
            .mark_feedback_locally_deleted(
                &ArtifactId::from("artifact-a"),
                &OrgId::from("acme"),
                &FeedbackId::from("feedback-a"),
            )
            .await
            .expect("local delete")
    );
    db::interact(&pool, move |conn| {
        conn.execute("UPDATE provider_delivery_outbox SET delivery_kind = 'discussion_thread', ordering_key = 'discussion:artifact-a:1', state = 'leased', attempts = 1, lease_owner = 'worker-a', lease_token = 'token-a', lease_expires_at = 1000, lease_version = 1 WHERE id = 'outbox-a'", []).expect("lease");
        Ok(())
    }).await.expect("lease");
    assert!(
        store
            .accept_delivery_and_record_message(AcceptedDiscussionDelivery {
                outbox_id: "outbox-a".into(),
                worker: "worker-a".into(),
                lease_token: "token-a".into(),
                lease_version: 1,
                external_thread_id: "thread-a".into(),
                external_message_id: "message-a".into(),
                now_millis: 10
            })
            .await
            .expect("accepted")
    );
    let conn = db::checkout(&pool).expect("checkout");
    assert_eq!(
        scalar::<String>(
            &conn,
            "SELECT state FROM provider_delivery_outbox WHERE id = 'outbox-a'"
        ),
        "accepted"
    );
    assert_eq!(
        scalar::<String>(
            &conn,
            "SELECT external_message_id FROM discussion_message_links WHERE outbox_id = 'outbox-a'"
        ),
        "message-a"
    );
    assert_eq!(
        scalar::<String>(
            &conn,
            "SELECT state FROM discussion_message_links WHERE outbox_id = 'outbox-a'"
        ),
        "tombstone_pending",
        "accepting a post never resurrects text deleted before provider delivery"
    );
    assert_eq!(
        scalar::<String>(
            &conn,
            "SELECT state FROM artifact_discussions WHERE artifact_id = 'artifact-a'"
        ),
        "connected"
    );
}

#[tokio::test]
async fn stale_connection_generation_or_reply_thread_never_partially_accepts() {
    let (_dir, pool, store) = store("u79-stale-acceptance");
    seed(&pool, "acme", "artifact-a", Some("feedback-a")).await;
    store
        .upsert_connection(CreateDiscussionConnection {
            org: "acme".into(),
            url: "https://discord.com/api/webhooks/123456/secret".into(),
            label: "Forum".into(),
        })
        .await
        .expect("connection");
    let first = store
        .enable_mirror(
            &ArtifactId::from("artifact-a"),
            &OrgId::from("acme"),
            "owner@example.test",
        )
        .await
        .expect("enable generation one");
    let connection_id = first.connection_id.expect("connection id");
    outbox(&pool, "stale-connection", "event-1", "acme", "connection-a").await;
    store
        .create_message_link(CreateDiscussionMessageLink {
            artifact_id: "artifact-a".into(),
            org: "acme".into(),
            connection_id: connection_id.clone(),
            feedback_id: "feedback-a".into(),
            delivery_event_id: "event-1".into(),
            outbox_id: "stale-connection".into(),
            external_thread_id: None,
            generation: 1,
        })
        .await
        .expect("stale connection link");
    lease_discussion_outbox(
        &pool,
        "stale-connection",
        "discussion_thread",
        "discussion:artifact-a:1",
        "token-1",
    )
    .await;
    store
        .disable_mirror(&ArtifactId::from("artifact-a"), &OrgId::from("acme"))
        .await
        .expect("disable");
    assert!(
        store
            .accept_delivery_and_record_message(AcceptedDiscussionDelivery {
                outbox_id: "stale-connection".into(),
                worker: "worker-a".into(),
                lease_token: "token-1".into(),
                lease_version: 1,
                external_thread_id: "thread-1".into(),
                external_message_id: "message-1".into(),
                now_millis: 10,
            })
            .await
            .expect("committed work drains after disable")
    );

    let second = store
        .enable_mirror(
            &ArtifactId::from("artifact-a"),
            &OrgId::from("acme"),
            "owner@example.test",
        )
        .await
        .expect("enable generation two");
    assert_eq!(second.generation, 2);
    assert!(second.thread_id.is_none());
    assert!(second.root_message_id.is_none());
    db::interact(&pool, move |conn| {
        conn.execute(
            "INSERT INTO feedback (id, artifact_id, org, viewer_email, body, artifact_revision) VALUES ('feedback-b', 'artifact-a', 'acme', 'viewer@example.test', 'comment', 1)",
            [],
        )
        .expect("feedback b");
        Ok(())
    })
    .await
    .expect("feedback b");
    outbox(&pool, "stale-generation", "event-2", "acme", "connection-a").await;
    store
        .create_message_link(CreateDiscussionMessageLink {
            artifact_id: "artifact-a".into(),
            org: "acme".into(),
            connection_id: connection_id.clone(),
            feedback_id: "feedback-b".into(),
            delivery_event_id: "event-2".into(),
            outbox_id: "stale-generation".into(),
            external_thread_id: None,
            generation: 1,
        })
        .await
        .expect("stale generation link");
    lease_discussion_outbox(
        &pool,
        "stale-generation",
        "discussion_thread",
        "discussion:artifact-a:1",
        "token-2",
    )
    .await;
    assert!(
        store
            .accept_delivery_and_record_message(AcceptedDiscussionDelivery {
                outbox_id: "stale-generation".into(),
                worker: "worker-a".into(),
                lease_token: "token-2".into(),
                lease_version: 1,
                external_thread_id: "thread-1".into(),
                external_message_id: "message-2".into(),
                now_millis: 10,
            })
            .await
            .expect("accepted old generation is finalized")
    );
    let discussion_after_race = store
        .get_discussion(&ArtifactId::from("artifact-a"), &OrgId::from("acme"))
        .await
        .expect("discussion")
        .expect("row");
    assert_eq!(discussion_after_race.generation, 2);
    assert!(discussion_after_race.thread_id.is_none());
    assert!(discussion_after_race.root_message_id.is_none());

    outbox(
        &pool,
        "root-generation-two",
        "event-3",
        "acme",
        "connection-a",
    )
    .await;
    store
        .create_message_link(CreateDiscussionMessageLink {
            artifact_id: "artifact-a".into(),
            org: "acme".into(),
            connection_id: connection_id.clone(),
            feedback_id: "feedback-a".into(),
            delivery_event_id: "event-3".into(),
            outbox_id: "root-generation-two".into(),
            external_thread_id: None,
            generation: 2,
        })
        .await
        .expect("root link");
    lease_discussion_outbox(
        &pool,
        "root-generation-two",
        "discussion_thread",
        "discussion:artifact-a:2",
        "token-3",
    )
    .await;
    assert!(
        store
            .accept_delivery_and_record_message(AcceptedDiscussionDelivery {
                outbox_id: "root-generation-two".into(),
                worker: "worker-a".into(),
                lease_token: "token-3".into(),
                lease_version: 1,
                external_thread_id: "thread-2".into(),
                external_message_id: "message-3".into(),
                now_millis: 10,
            })
            .await
            .expect("root accepted")
    );

    outbox(
        &pool,
        "wrong-reply-thread",
        "event-4",
        "acme",
        "connection-a",
    )
    .await;
    store
        .create_message_link(CreateDiscussionMessageLink {
            artifact_id: "artifact-a".into(),
            org: "acme".into(),
            connection_id,
            feedback_id: "feedback-b".into(),
            delivery_event_id: "event-4".into(),
            outbox_id: "wrong-reply-thread".into(),
            external_thread_id: Some("thread-2".into()),
            generation: 2,
        })
        .await
        .expect("reply link");
    lease_discussion_outbox(
        &pool,
        "wrong-reply-thread",
        "discussion_message",
        "discussion:artifact-a:2",
        "token-4",
    )
    .await;
    assert!(
        store
            .accept_delivery_and_record_message(AcceptedDiscussionDelivery {
                outbox_id: "wrong-reply-thread".into(),
                worker: "worker-a".into(),
                lease_token: "token-4".into(),
                lease_version: 1,
                external_thread_id: "wrong-thread".into(),
                external_message_id: "message-4".into(),
                now_millis: 10,
            })
            .await
            .is_err()
    );

    let conn = db::checkout(&pool).expect("checkout");
    assert_eq!(
        scalar::<String>(
            &conn,
            "SELECT state FROM provider_delivery_outbox WHERE id = 'stale-connection'"
        ),
        "accepted",
        "work committed before disable must drain"
    );
    assert_eq!(
        scalar::<String>(
            &conn,
            "SELECT state FROM provider_delivery_outbox WHERE id = 'stale-generation'"
        ),
        "accepted",
        "a provider-accepted old generation is finalized without changing generation two"
    );
    let id = "wrong-reply-thread";
    assert_eq!(
        scalar::<String>(
            &conn,
            &format!("SELECT state FROM provider_delivery_outbox WHERE id = '{id}'")
        ),
        "leased",
        "{id} must remain unaccepted"
    );
    assert_eq!(
        scalar::<String>(
            &conn,
            &format!("SELECT state FROM discussion_message_links WHERE outbox_id = '{id}'")
        ),
        "pending",
        "{id} mapping must remain untouched"
    );
}

#[tokio::test]
async fn tombstone_delivery_retains_post_mapping_and_requires_current_authority() {
    let (_dir, pool, store) = store("u79-tombstone");
    seed(&pool, "acme", "artifact-a", Some("feedback-a")).await;
    store
        .upsert_connection(CreateDiscussionConnection {
            org: "acme".into(),
            url: "https://discord.com/api/webhooks/123456/secret".into(),
            label: "Forum".into(),
        })
        .await
        .expect("connection");
    let discussion = store
        .enable_mirror(
            &ArtifactId::from("artifact-a"),
            &OrgId::from("acme"),
            "owner@example.test",
        )
        .await
        .expect("enable");
    let connection_id = discussion.connection_id.expect("connection id");
    outbox(&pool, "post-a", "event-a", "acme", "connection-a").await;
    store
        .create_message_link(CreateDiscussionMessageLink {
            artifact_id: "artifact-a".into(),
            org: "acme".into(),
            connection_id: connection_id.clone(),
            feedback_id: "feedback-a".into(),
            delivery_event_id: "event-a".into(),
            outbox_id: "post-a".into(),
            external_thread_id: None,
            generation: 1,
        })
        .await
        .expect("post mapping");
    lease_discussion_outbox(
        &pool,
        "post-a",
        "discussion_thread",
        "discussion:artifact-a:1",
        "post-token",
    )
    .await;
    store
        .accept_delivery_and_record_message(AcceptedDiscussionDelivery {
            outbox_id: "post-a".into(),
            worker: "worker-a".into(),
            lease_token: "post-token".into(),
            lease_version: 1,
            external_thread_id: "thread-a".into(),
            external_message_id: "message-a".into(),
            now_millis: 10,
        })
        .await
        .expect("post accepted");
    assert!(
        store
            .mark_feedback_locally_deleted(
                &ArtifactId::from("artifact-a"),
                &OrgId::from("acme"),
                &FeedbackId::from("feedback-a"),
            )
            .await
            .expect("mark deleted")
    );

    discussion_outbox(
        &pool,
        "tombstone-a",
        "event-tombstone",
        "acme",
        &connection_id,
    )
    .await;
    lease_discussion_outbox(
        &pool,
        "tombstone-a",
        "discussion_tombstone",
        "discussion:artifact-a:1",
        "tombstone-token",
    )
    .await;
    let expected_secret = format!("discussion:{connection_id}");
    set_tombstone_contract(
        &pool,
        "acme",
        "wrong-connection",
        &expected_secret,
        Some("post-a"),
    )
    .await;
    for (tenant, target, secret) in [
        ("acme", "wrong-connection", expected_secret.as_str()),
        (
            "acme",
            connection_id.as_str(),
            "discussion:wrong-connection",
        ),
        ("other", connection_id.as_str(), expected_secret.as_str()),
    ] {
        set_tombstone_contract(&pool, tenant, target, secret, Some("post-a")).await;
        assert!(
            store
                .bind_tombstone_delivery(BindDiscussionTombstone {
                    artifact_id: "artifact-a".into(),
                    org: "acme".into(),
                    feedback_id: "feedback-a".into(),
                    connection_id: connection_id.clone(),
                    generation: 1,
                    outbox_id: "tombstone-a".into(),
                })
                .await
                .is_err(),
            "wrong tenant, target, or secret must not bind a tombstone job"
        );
    }
    set_tombstone_contract(
        &pool,
        "acme",
        &connection_id,
        &expected_secret,
        Some("post-a"),
    )
    .await;
    assert!(
        store
            .bind_tombstone_delivery(BindDiscussionTombstone {
                artifact_id: "artifact-a".into(),
                org: "other".into(),
                feedback_id: "feedback-a".into(),
                connection_id: connection_id.clone(),
                generation: 1,
                outbox_id: "tombstone-a".into(),
            })
            .await
            .is_err(),
        "a tombstone job cannot bind across tenants"
    );
    for (connection_id, generation) in [("wrong-connection", 1), (connection_id.as_str(), 2)] {
        assert!(
            store
                .bind_tombstone_delivery(BindDiscussionTombstone {
                    artifact_id: "artifact-a".into(),
                    org: "acme".into(),
                    feedback_id: "feedback-a".into(),
                    connection_id: connection_id.to_owned(),
                    generation,
                    outbox_id: "tombstone-a".into(),
                })
                .await
                .is_err(),
            "wrong connection or generation must not bind a tombstone job"
        );
    }
    let bound = store
        .bind_tombstone_delivery(BindDiscussionTombstone {
            artifact_id: "artifact-a".into(),
            org: "acme".into(),
            feedback_id: "feedback-a".into(),
            connection_id,
            generation: 1,
            outbox_id: "tombstone-a".into(),
        })
        .await
        .expect("bind tombstone");
    assert_eq!(bound.outbox_id, "post-a", "post mapping is immutable");
    assert_eq!(bound.tombstone_outbox_id.as_deref(), Some("tombstone-a"));
    assert_eq!(bound.state, "tombstone_pending");

    assert!(
        !store
            .accept_tombstone_delivery(AcceptedDiscussionDelivery {
                outbox_id: "tombstone-a".into(),
                worker: "worker-a".into(),
                lease_token: "stale-token".into(),
                lease_version: 1,
                external_thread_id: "thread-a".into(),
                external_message_id: "message-a".into(),
                now_millis: 11,
            })
            .await
            .expect("stale lease returns false")
    );
    {
        let conn = db::checkout(&pool).expect("checkout after stale lease");
        assert_eq!(
            scalar::<String>(
                &conn,
                "SELECT state FROM discussion_message_links WHERE outbox_id = 'post-a'"
            ),
            "tombstone_pending",
            "stale lease rolls back the retained link transition"
        );
        assert_eq!(
            scalar::<String>(
                &conn,
                "SELECT state FROM provider_delivery_outbox WHERE id = 'tombstone-a'"
            ),
            "leased"
        );
    }

    assert!(
        store
            .accept_tombstone_delivery(AcceptedDiscussionDelivery {
                outbox_id: "tombstone-a".into(),
                worker: "worker-a".into(),
                lease_token: "tombstone-token".into(),
                lease_version: 1,
                external_thread_id: "thread-a".into(),
                external_message_id: "message-a".into(),
                now_millis: 12,
            })
            .await
            .expect("accept tombstone")
    );
    let conn = db::checkout(&pool).expect("checkout after accept");
    assert_eq!(
        scalar::<String>(
            &conn,
            "SELECT state FROM discussion_message_links WHERE outbox_id = 'post-a'"
        ),
        "tombstoned"
    );
    assert_eq!(
        scalar::<String>(
            &conn,
            "SELECT external_message_id FROM discussion_message_links WHERE outbox_id = 'post-a'"
        ),
        "message-a"
    );
    assert_eq!(
        scalar::<String>(
            &conn,
            "SELECT state FROM provider_delivery_outbox WHERE id = 'post-a'"
        ),
        "accepted"
    );
    assert_eq!(
        scalar::<String>(
            &conn,
            "SELECT state FROM provider_delivery_outbox WHERE id = 'tombstone-a'"
        ),
        "accepted"
    );
}
