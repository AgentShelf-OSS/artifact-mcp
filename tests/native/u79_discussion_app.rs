//! PBI-079 APP1: feedback mutations atomically plan optional discussion work.

use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use artifact_mcp::{
    config::SequentialIdSource,
    error::AppError,
    integrations::delivery_envelope::stable_delivery_event_id,
    integrations::discussion_envelope::{
        DiscordDiscussionEnvelopeV1, DiscordDiscussionOperationV1,
    },
    model::{
        ArtifactId, ArtifactMeta, ClientId, EmailAddress, FeedbackId, OrgId, SubmitFeedback,
        Timestamp, Viewer, WebhookEvent,
    },
    persistence::{
        feedback_delivery,
        feedback_delivery::DeliveryPlanningContext,
        migrations::{self, MigrationContext},
        outbox::{OutboxClock, OutboxIdGenerator},
    },
};
use rusqlite::{Connection, params};

const ORG: &str = "acme";
const ARTIFACT: &str = "discussion-artifact";
const BASE: &str = "https://artifacts.test";
type PlannedRow = (String, String, String, Option<String>, Vec<u8>);

struct FixedClock;
impl OutboxClock for FixedClock {
    fn now_millis(&self) -> i64 {
        1_800_000_000_000
    }
}

#[derive(Default)]
struct SequenceIds(AtomicU64);
impl OutboxIdGenerator for SequenceIds {
    fn next_id(&self) -> String {
        format!(
            "discussion-outbox-{}",
            self.0.fetch_add(1, Ordering::Relaxed)
        )
    }
}

struct Fixture {
    conn: Connection,
    ids: SequentialIdSource,
    planning: DeliveryPlanningContext,
    meta: ArtifactMeta,
}

impl Fixture {
    fn new() -> Self {
        let mut conn = Connection::open_in_memory().expect("db");
        conn.execute_batch("PRAGMA foreign_keys=ON").expect("fk");
        migrations::apply(&mut conn, &MigrationContext::empty()).expect("migrate");
        conn.execute("INSERT INTO orgs (name) VALUES (?1)", [ORG])
            .expect("org");
        conn.execute(
            "INSERT INTO artifacts (id, client_id, org, title, description, bytes, created_at, updated_at, uploader_label, owner_email, is_bundle, entry, revision, category, hidden, body_sha256) \
             VALUES (?1, 'publisher', ?2, 'Discussed artifact', '', 1, '2026-01-01', '2026-01-01', 'publisher', 'owner@example.test', 0, '', 7, 'Docs', 0, ?3)",
            params![ARTIFACT, ORG, "a".repeat(64)],
        ).expect("artifact");
        Self {
            conn,
            ids: SequentialIdSource::starting_at(1),
            planning: DeliveryPlanningContext::new(
                Arc::new(FixedClock),
                Arc::new(SequenceIds::default()),
            ),
            meta: ArtifactMeta {
                id: ArtifactId::from(ARTIFACT),
                client_id: ClientId::from("publisher"),
                org: OrgId::from(ORG),
                title: "Discussed artifact".into(),
                description: String::new(),
                bytes: 1,
                created_at: Timestamp("2026-01-01".into()),
                updated_at: Timestamp("2026-01-01".into()),
                uploader_label: "publisher".into(),
                owner_email: Some("owner@example.test".into()),
                is_bundle: false,
                entry: String::new(),
                revision: 7,
                category: "Docs".into(),
                hidden: false,
                body_sha256: "a".repeat(64),
            },
        }
    }

    fn enable(&self) {
        self.conn.execute(
            "INSERT INTO org_discord_discussion_connections (id, org, url, label) VALUES ('connection-a', ?1, 'https://discord.com/api/webhooks/123/token-not-in-payload', 'Forum')",
            [ORG],
        ).expect("connection");
        self.conn.execute(
            "INSERT INTO artifact_discussions (artifact_id, org, provider, mode, connection_org, connection_id, state, generation) VALUES (?1, ?2, 'discord', 'discord_mirror', ?2, 'connection-a', 'pending', 1)",
            params![ARTIFACT, ORG],
        ).expect("discussion");
    }

    fn enable_notification_thread(&self) {
        let event_id = stable_delivery_event_id(
            &OrgId::from(ORG),
            &WebhookEvent::Published,
            &format!("artifact:{ARTIFACT}:1"),
        );
        self.conn
            .execute(
                "INSERT INTO org_webhooks (id, org, url, label, events) \
                 VALUES ('webhook-a', ?1, 'https://discord.com/api/webhooks/123/anchor-token', 'Artifacts', 'published,feedback,resolved')",
                [ORG],
            )
            .expect("anchor webhook");
        self.conn
            .execute(
                "INSERT INTO org_webhooks (id, org, url, label, events) \
                 VALUES ('webhook-b', ?1, 'https://discord.com/api/webhooks/456/other-token', 'Feedback', 'feedback,resolved')",
                [ORG],
            )
            .expect("other webhook");
        self.conn
            .execute(
                "INSERT INTO provider_delivery_outbox \
                 (id, provider, event_id, tenant, event_type, target_key, bucket_id, secret_ref, \
                  payload, payload_sha256, state, next_attempt_at, discord_message_id, \
                  created_at, updated_at, completed_at) \
                 VALUES ('publication-anchor', 'discord', ?1, ?2, 'published', 'webhook-a', \
                  'webhook-a', 'webhook:webhook-a', x'7B7D', \
                  '44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a', \
                  'accepted', 0, '223456789012345678', 0, 0, 0)",
                params![event_id, ORG],
            )
            .expect("publication anchor");
        self.conn
            .execute(
                "INSERT INTO org_discord_discussion_connections \
                 (id, org, url, label, strategy, notification_webhook_id, \
                  notification_provider_webhook_id, channel_id, guild_id) \
                 VALUES ('connection-a', ?1, '', 'Artifact threads', 'notification_thread', \
                  'webhook-a', '423456789012345678', '123456789012345678', \
                  '323456789012345678')",
                [ORG],
            )
            .expect("notification connection");
        self.conn
            .execute(
                "INSERT INTO artifact_discussions \
                 (artifact_id, org, provider, mode, connection_org, connection_id, state, \
                  generation, anchor_outbox_id) \
                 VALUES (?1, ?2, 'discord', 'discord_mirror', ?2, 'connection-a', 'pending', 1, \
                  'publication-anchor')",
                params![ARTIFACT, ORG],
            )
            .expect("notification discussion");
    }

    fn enable_inherited_notification_thread(&self) {
        self.enable_notification_thread();
        self.conn
            .execute(
                "DELETE FROM artifact_discussions WHERE artifact_id=?1 AND org=?2",
                params![ARTIFACT, ORG],
            )
            .expect("remove manual artifact mode");
        self.conn
            .execute(
                "INSERT INTO org_discord_threading_policies (org, outbound_enabled) VALUES (?1, 1)",
                [ORG],
            )
            .expect("organization policy");
    }

    fn submit(&mut self, body: &str) -> FeedbackId {
        self.submit_with_base(BASE, body)
    }

    fn submit_with_base(&mut self, public_base_url: &str, body: &str) -> FeedbackId {
        feedback_delivery::submit(
            &mut self.conn,
            &self.ids,
            &self.planning,
            public_base_url,
            &self.meta,
            &SubmitFeedback {
                viewer_email: EmailAddress::from("viewer@example.test"),
                body: body.into(),
                parent_id: None,
                anchor: None,
                anchor_path: None,
                anchor_page: None,
            },
            4_000,
        )
        .expect("submit")
        .id
    }

    fn viewer() -> Viewer {
        Viewer {
            email: Some(EmailAddress::from("viewer@example.test")),
            is_admin: false,
            org: None,
        }
    }
}

#[test]
fn first_opted_in_feedback_creates_one_root_then_direct_root_replies() {
    let mut fixture = Fixture::new();
    fixture.enable();
    let first = fixture.submit("first mirrored comment");
    let second = fixture.submit("second mirrored comment");
    let mut rows = fixture.conn.prepare(
        "SELECT id, event_id, delivery_kind, depends_on_outbox_id, payload FROM provider_delivery_outbox ORDER BY created_at, id",
    ).expect("query");
    let rows: Vec<PlannedRow> = rows
        .query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        })
        .expect("rows")
        .map(Result::unwrap)
        .collect();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].2, "discussion_thread");
    assert_eq!(rows[1].2, "discussion_message");
    assert_eq!(rows[1].3.as_deref(), Some(rows[0].0.as_str()));
    let envelope = DiscordDiscussionEnvelopeV1::decode_canonical(
        &rows[0].4,
        &OrgId::from(ORG),
        &rows[0].1,
        ARTIFACT,
        "connection-a",
        1,
        None,
    )
    .expect("canonical root envelope");
    assert_eq!(envelope.operation().feedback_id(), first.0);
    assert!(matches!(
        envelope.operation(),
        DiscordDiscussionOperationV1::Thread { .. }
    ));
    let payload = String::from_utf8(rows[0].4.clone()).expect("json");
    assert!(payload.contains(BASE));
    assert!(payload.contains("Discussed artifact"));
    assert!(!payload.contains("token-not-in-payload"));
    assert_eq!(
        fixture
            .conn
            .query_row("SELECT COUNT(*) FROM discussion_message_links", [], |r| r
                .get::<_, i64>(
                0
            ))
            .expect("links"),
        2
    );
    assert_ne!(first, second);
}

#[test]
fn inherited_policy_materializes_one_root_without_per_artifact_enablement() {
    let mut fixture = Fixture::new();
    fixture.enable_inherited_notification_thread();

    let first = fixture.submit("first inherited comment");
    let second = fixture.submit("second inherited comment");

    let discussion: (String, String, i64, String) = fixture
        .conn
        .query_row(
            "SELECT mode, state, generation, enabled_by FROM artifact_discussions \
             WHERE artifact_id=?1 AND org=?2",
            params![ARTIFACT, ORG],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .expect("inherited discussion");
    assert_eq!(
        discussion,
        (
            "discord_mirror".to_owned(),
            "pending".to_owned(),
            1,
            "organization-policy".to_owned(),
        )
    );
    assert_eq!(
        fixture
            .conn
            .query_row(
                "SELECT COUNT(*) FROM provider_delivery_outbox \
                 WHERE delivery_kind='discussion_thread' AND tenant=?1",
                [ORG],
                |row| row.get::<_, i64>(0),
            )
            .expect("root count"),
        1,
        "sequential first-comment transactions share one generation-scoped root"
    );
    assert_eq!(
        fixture
            .conn
            .query_row(
                "SELECT COUNT(*) FROM discussion_message_links \
                 WHERE artifact_id=?1 AND org=?2 AND feedback_id IN (?3, ?4)",
                params![ARTIFACT, ORG, first.0, second.0],
                |row| row.get::<_, i64>(0),
            )
            .expect("correlated comments"),
        2
    );
    assert_eq!(
        fixture
            .conn
            .query_row(
                "SELECT COUNT(*) FROM provider_delivery_outbox \
                 WHERE tenant=?1 AND target_key='webhook-a' AND event_type='feedback'",
                [ORG],
                |row| row.get::<_, i64>(0),
            )
            .expect("selected webhook suppression"),
        0,
        "the selected publication webhook does not receive standalone feedback cards"
    );
}

#[test]
fn inherited_override_and_disable_boundaries_keep_feedback_local() {
    let mut fixture = Fixture::new();
    fixture.enable_inherited_notification_thread();
    fixture
        .conn
        .execute(
            "INSERT INTO artifact_discussion_overrides (artifact_id, org, mode) \
             VALUES (?1, ?2, 'artifact_only')",
            params![ARTIFACT, ORG],
        )
        .expect("artifact-only exception");

    let local = fixture.submit("kept in Artifact MCP");
    assert_eq!(
        fixture
            .conn
            .query_row(
                "SELECT COUNT(*) FROM discussion_message_links WHERE feedback_id=?1",
                [&local.0],
                |row| row.get::<_, i64>(0),
            )
            .expect("local link count"),
        0
    );

    fixture
        .conn
        .execute(
            "DELETE FROM artifact_discussion_overrides WHERE artifact_id=?1 AND org=?2",
            params![ARTIFACT, ORG],
        )
        .expect("reset to organization default");
    let inherited = fixture.submit("organization default");
    assert_eq!(
        fixture
            .conn
            .query_row(
                "SELECT COUNT(*) FROM discussion_message_links WHERE feedback_id=?1",
                [&inherited.0],
                |row| row.get::<_, i64>(0),
            )
            .expect("inherited link count"),
        1
    );

    fixture
        .conn
        .execute(
            "UPDATE org_discord_threading_policies SET outbound_enabled=0 WHERE org=?1",
            [ORG],
        )
        .expect("disable organization policy");
    let after_disable = fixture.submit("canonical after disable");
    assert_eq!(
        fixture
            .conn
            .query_row(
                "SELECT COUNT(*) FROM discussion_message_links WHERE feedback_id=?1",
                [&after_disable.0],
                |row| row.get::<_, i64>(0),
            )
            .expect("disabled link count"),
        0
    );
    assert_eq!(
        fixture
            .conn
            .query_row(
                "SELECT COUNT(*) FROM feedback WHERE id IN (?1, ?2, ?3)",
                params![local.0, inherited.0, after_disable.0],
                |row| row.get::<_, i64>(0),
            )
            .expect("canonical feedback"),
        3
    );
}

#[test]
fn notification_thread_waits_for_publication_and_suppresses_only_duplicate_webhook_event() {
    let mut fixture = Fixture::new();
    fixture.enable_notification_thread();
    fixture.submit("first anchored comment");

    let root: (String, Option<String>) = fixture
        .conn
        .query_row(
            "SELECT delivery_kind, depends_on_outbox_id FROM provider_delivery_outbox \
             WHERE delivery_kind = 'discussion_thread'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("discussion root");
    assert_eq!(root.0, "discussion_thread");
    assert_eq!(root.1.as_deref(), Some("publication-anchor"));

    let feedback_targets: Vec<String> = fixture
        .conn
        .prepare(
            "SELECT target_key FROM provider_delivery_outbox \
             WHERE delivery_kind = 'event' AND event_type = 'feedback' ORDER BY target_key",
        )
        .expect("feedback query")
        .query_map([], |row| row.get(0))
        .expect("feedback rows")
        .map(Result::unwrap)
        .collect();
    assert_eq!(feedback_targets, vec!["webhook-b"]);
}

#[test]
fn oversized_public_base_url_cannot_overflow_or_rollback_discussion_content() {
    let mut fixture = Fixture::new();
    fixture.enable();
    let long_base = format!("https://artifacts.test/{}", "a".repeat(2_100));

    let created = fixture.submit_with_base(&long_base, "accepted local feedback");
    let (event_id, payload): (String, Vec<u8>) = fixture
        .conn
        .query_row(
            "SELECT event_id, payload FROM provider_delivery_outbox WHERE delivery_kind = 'discussion_thread'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("discussion row");
    let envelope = DiscordDiscussionEnvelopeV1::decode_canonical(
        &payload,
        &OrgId::from(ORG),
        &event_id,
        ARTIFACT,
        "connection-a",
        1,
        None,
    )
    .expect("bounded canonical envelope");
    let DiscordDiscussionOperationV1::Thread { content, .. } = envelope.operation() else {
        panic!("expected thread operation");
    };

    assert_eq!(envelope.operation().feedback_id(), created.0);
    assert!(content.chars().count() <= 2_000);
    assert!(content.contains("accepted local feedback"));
    assert!(content.contains("Artifact link unavailable"));
}

#[test]
fn long_deep_link_is_preserved_when_the_complete_message_still_fits() {
    let mut fixture = Fixture::new();
    fixture.enable();
    let long_base = format!("https://artifacts.test/{}", "a".repeat(1_100));

    fixture.submit_with_base(&long_base, "still linked");
    let (event_id, payload): (String, Vec<u8>) = fixture
        .conn
        .query_row(
            "SELECT event_id, payload FROM provider_delivery_outbox WHERE delivery_kind = 'discussion_thread'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("discussion row");
    let envelope = DiscordDiscussionEnvelopeV1::decode_canonical(
        &payload,
        &OrgId::from(ORG),
        &event_id,
        ARTIFACT,
        "connection-a",
        1,
        None,
    )
    .expect("bounded canonical envelope");
    let DiscordDiscussionOperationV1::Thread { content, .. } = envelope.operation() else {
        panic!("expected thread operation");
    };

    assert!(content.contains(&long_base));
    assert!(content.contains("still linked"));
    assert!(!content.contains("Artifact link unavailable"));
}

#[test]
fn absent_or_paused_mirror_never_plans_new_feedback_content() {
    let mut fixture = Fixture::new();
    fixture.submit("local only");
    assert_eq!(
        fixture
            .conn
            .query_row("SELECT COUNT(*) FROM provider_delivery_outbox", [], |r| r
                .get::<_, i64>(
                0
            ))
            .expect("local count"),
        0
    );
    fixture.enable();
    fixture
        .conn
        .execute(
            "UPDATE artifact_discussions SET mode='artifact_only', state='paused' WHERE artifact_id=?1",
            [ARTIFACT],
        )
        .expect("pause");
    fixture.submit("still local only");
    assert_eq!(
        fixture
            .conn
            .query_row("SELECT COUNT(*) FROM provider_delivery_outbox", [], |r| r
                .get::<_, i64>(
                0
            ))
            .expect("paused count"),
        0
    );
}

#[test]
fn discussion_queue_admission_failure_rolls_back_the_feedback_and_link() {
    let mut fixture = Fixture::new();
    fixture.enable();
    for number in 0..1_000 {
        let id = format!("capacity-{number}");
        fixture
            .conn
            .execute(
                "INSERT INTO provider_delivery_outbox \
                 (id, provider, event_id, tenant, event_type, target_key, bucket_id, secret_ref, payload, payload_sha256, state, next_attempt_at, created_at, updated_at) \
                 VALUES (?1, 'discord', ?1, ?2, 'event', ?1, ?1, ?3, x'7B7D', ?4, 'ready', 0, 0, 0)",
                params![id, ORG, format!("webhook:capacity-{number}"), "44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a"],
            )
            .expect("fill queue");
    }
    assert_eq!(
        feedback_delivery::submit(
            &mut fixture.conn,
            &fixture.ids,
            &fixture.planning,
            BASE,
            &fixture.meta,
            &SubmitFeedback {
                viewer_email: EmailAddress::from("viewer@example.test"),
                body: "must roll back".into(),
                parent_id: None,
                anchor: None,
                anchor_path: None,
                anchor_page: None,
            },
            4_000,
        ),
        Err(AppError::RateLimited)
    );
    assert_eq!(
        fixture
            .conn
            .query_row("SELECT COUNT(*) FROM feedback", [], |r| r.get::<_, i64>(0))
            .expect("feedback"),
        0
    );
    assert_eq!(
        fixture
            .conn
            .query_row("SELECT COUNT(*) FROM discussion_message_links", [], |r| r
                .get::<_, i64>(
                0
            ))
            .expect("links"),
        0
    );
}

#[test]
fn markers_are_link_free_and_only_follow_real_state_transitions() {
    let mut fixture = Fixture::new();
    fixture.enable();
    let id = fixture.submit("comment");
    assert!(
        feedback_delivery::resolve_as_viewer(
            &mut fixture.conn,
            &fixture.planning,
            BASE,
            &fixture.meta,
            &Fixture::viewer(),
            id.clone()
        )
        .expect("resolve")
        .changed
    );
    assert!(
        !feedback_delivery::resolve_as_viewer(
            &mut fixture.conn,
            &fixture.planning,
            BASE,
            &fixture.meta,
            &Fixture::viewer(),
            id.clone()
        )
        .expect("resolve noop")
        .changed
    );
    assert!(
        feedback_delivery::reopen_as_publisher_with_delivery(
            &mut fixture.conn,
            &fixture.planning,
            &fixture.meta,
            id
        )
        .expect("reopen")
    );
    assert_eq!(
        fixture
            .conn
            .query_row("SELECT COUNT(*) FROM discussion_message_links", [], |r| r
                .get::<_, i64>(
                0
            ))
            .expect("links"),
        1
    );
    assert_eq!(fixture.conn.query_row("SELECT COUNT(*) FROM provider_delivery_outbox WHERE event_type IN ('discussion_resolved', 'discussion_reopened')", [], |r| r.get::<_, i64>(0)).expect("markers"), 2);
}

#[test]
fn paused_mirror_can_tombstone_committed_content() {
    let mut fixture = Fixture::new();
    fixture.enable();
    let id = fixture.submit("delete me");
    fixture.conn.execute("UPDATE artifact_discussions SET mode='artifact_only', state='paused' WHERE artifact_id=?1", [ARTIFACT]).expect("pause");
    let deleted = feedback_delivery::delete_as_viewer_with_delivery(
        &mut fixture.conn,
        &fixture.planning,
        &fixture.meta,
        &Fixture::viewer(),
        id,
    )
    .expect("delete");
    assert!(deleted.changed);
    assert_eq!(
        fixture
            .conn
            .query_row("SELECT state FROM discussion_message_links", [], |r| r
                .get::<_, String>(
                0
            ))
            .expect("link state"),
        "local_deleted"
    );
    assert_eq!(fixture.conn.query_row("SELECT COUNT(*) FROM provider_delivery_outbox WHERE delivery_kind='discussion_tombstone'", [], |r| r.get::<_, i64>(0)).expect("tombstone"), 1);
}

#[test]
fn reenabled_generation_rejects_a_stale_link_tombstone() {
    let mut fixture = Fixture::new();
    fixture.enable();
    let id = fixture.submit("stale generation");
    fixture.conn.execute(
        "UPDATE artifact_discussions SET mode='discord_mirror', state='pending', generation=2, thread_id=NULL, root_message_id=NULL WHERE artifact_id=?1",
        [ARTIFACT],
    ).expect("re-enable");
    assert!(
        feedback_delivery::delete_as_viewer_with_delivery(
            &mut fixture.conn,
            &fixture.planning,
            &fixture.meta,
            &Fixture::viewer(),
            id,
        )
        .expect("delete")
        .changed
    );
    assert_eq!(fixture.conn.query_row("SELECT COUNT(*) FROM provider_delivery_outbox WHERE delivery_kind='discussion_tombstone'", [], |r| r.get::<_, i64>(0)).expect("tombstone"), 0);
}
