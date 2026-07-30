//! PBI-056 Stage D: feedback mutations and provider fanout share one SQLite commit.

use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use artifact_mcp::{
    config::SequentialIdSource,
    error::AppError,
    integrations::delivery_envelope::{DeliveryEnvelopeV1, stable_delivery_event_id},
    model::{
        ArtifactId, ArtifactMeta, ClientId, EmailAddress, FeedbackId, OrgId, SubmitFeedback,
        Timestamp, Viewer, WebhookEvent,
    },
    persistence::{
        feedback, feedback_delivery,
        feedback_delivery::DeliveryPlanningContext,
        migrations::{self, MigrationContext},
        outbox::{self, EnqueueDelivery, OutboxClock, OutboxIdGenerator},
    },
};
use rusqlite::{Connection, TransactionBehavior, params};

const ORG: &str = "acme";
const ARTIFACT: &str = "feedback0001";
const PUBLIC_BASE: &str = "https://artifacts.test";

struct FixedClock;

impl OutboxClock for FixedClock {
    fn now_millis(&self) -> i64 {
        1_800_000_000_000
    }
}

#[derive(Default)]
struct SequenceOutboxIds(AtomicU64);

impl OutboxIdGenerator for SequenceOutboxIds {
    fn next_id(&self) -> String {
        format!("outbox-{}", self.0.fetch_add(1, Ordering::Relaxed))
    }
}

struct Fixture {
    conn: Connection,
    feedback_ids: SequentialIdSource,
    planning: DeliveryPlanningContext,
    meta: ArtifactMeta,
}

impl Fixture {
    fn new() -> Self {
        let mut conn = Connection::open_in_memory().expect("database");
        conn.execute_batch("PRAGMA foreign_keys=ON")
            .expect("foreign keys");
        migrations::apply(&mut conn, &MigrationContext::empty()).expect("migrations");
        conn.execute("INSERT OR IGNORE INTO orgs (name) VALUES (?1)", [ORG])
            .expect("organization");
        conn.execute(
            "INSERT INTO artifacts \
             (id, client_id, org, title, description, bytes, created_at, updated_at, \
              uploader_label, owner_email, is_bundle, entry, revision, category, hidden, \
              body_sha256) \
             VALUES (?1, 'publisher', ?2, 'Persisted title', 'Persisted description', 42, \
                     '2026-07-30 00:00:00', '2026-07-30 00:00:00', 'Persisted publisher', \
                     'owner@example.test', 0, '', 3, 'Reviews', 0, ?3)",
            params![ARTIFACT, ORG, "a".repeat(64)],
        )
        .expect("artifact");
        Self {
            conn,
            feedback_ids: SequentialIdSource::starting_at(1),
            planning: DeliveryPlanningContext::new(
                Arc::new(FixedClock),
                Arc::new(SequenceOutboxIds::default()),
            ),
            meta: ArtifactMeta {
                id: ArtifactId::from(ARTIFACT),
                client_id: ClientId::from("publisher"),
                org: OrgId::from(ORG),
                title: "Stale route title".to_owned(),
                description: "Stale route description".to_owned(),
                bytes: 7,
                created_at: Timestamp("2026-07-29 00:00:00".to_owned()),
                updated_at: Timestamp("2026-07-29 00:00:00".to_owned()),
                uploader_label: "Stale publisher".to_owned(),
                owner_email: Some("owner@example.test".to_owned()),
                is_bundle: false,
                entry: String::new(),
                revision: 2,
                category: "Stale".to_owned(),
                hidden: false,
                body_sha256: "b".repeat(64),
            },
        }
    }

    fn subscribe(&self, id: &str, events: &str) {
        self.conn
            .execute(
                "INSERT INTO org_webhooks (id, org, url, events, created_at) \
                 VALUES (?1, ?2, 'https://discord.com/api/webhooks/123/secret-token', ?3, \
                         '2026-07-30 00:00:00')",
                params![id, ORG, events],
            )
            .expect("subscriber");
    }

    fn submit(&mut self, body: &str, parent_id: Option<FeedbackId>) -> FeedbackId {
        feedback_delivery::submit(
            &mut self.conn,
            &self.feedback_ids,
            &self.planning,
            PUBLIC_BASE,
            &self.meta,
            &SubmitFeedback {
                viewer_email: EmailAddress::from("viewer@example.test"),
                body: body.to_owned(),
                parent_id,
                anchor: None,
                anchor_path: None,
                anchor_page: None,
            },
            4_000,
        )
        .expect("submit")
        .id
    }

    fn viewer(&self) -> Viewer {
        Viewer {
            email: Some(EmailAddress::from("viewer@example.test")),
            org: Some(OrgId::from(ORG)),
            is_admin: false,
        }
    }

    fn outbox_count(&self) -> i64 {
        self.conn
            .query_row("SELECT COUNT(*) FROM provider_delivery_outbox", [], |row| {
                row.get(0)
            })
            .expect("outbox count")
    }

    fn feedback_count(&self) -> i64 {
        self.conn
            .query_row("SELECT COUNT(*) FROM feedback", [], |row| row.get(0))
            .expect("feedback count")
    }
}

#[test]
fn submit_and_reply_commit_with_canonical_subscriber_fanout_from_persisted_rows() {
    let mut fixture = Fixture::new();
    fixture.subscribe("feedback-one", "feedback");
    fixture.subscribe("feedback-two", "published,feedback");
    fixture.subscribe("resolved-only", "resolved");

    let parent = fixture.submit(" Parent body ", None);
    assert_eq!(fixture.feedback_count(), 1);
    assert_eq!(fixture.outbox_count(), 2);
    let expected_parent = stable_delivery_event_id(
        &OrgId::from(ORG),
        &WebhookEvent::Feedback,
        &format!("feedback:{parent}"),
    );
    let rows = delivery_rows(&fixture.conn, "feedback");
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().all(|row| row.event_id == expected_parent));
    assert_eq!(
        rows.iter()
            .map(|row| row.target.as_str())
            .collect::<Vec<_>>(),
        ["feedback-one", "feedback-two"]
    );
    let envelope = DeliveryEnvelopeV1::decode_canonical(
        &rows[0].payload,
        &OrgId::from(ORG),
        &WebhookEvent::Feedback,
        &rows[0].event_id,
        Some(&rows[0].digest),
    )
    .expect("canonical envelope");
    let discord: serde_json::Value =
        serde_json::from_slice(&envelope.discord_request_body_bytes().expect("discord body"))
            .expect("discord JSON");
    assert_eq!(discord["embeds"][0]["title"], "Persisted title");
    assert_eq!(discord["embeds"][0]["description"], "Parent body");
    assert_eq!(
        discord["embeds"][0]["url"],
        format!("{PUBLIC_BASE}/{ARTIFACT}")
    );
    assert!(
        rows.iter()
            .all(|row| row.secret_ref == format!("webhook:{}", row.target))
    );
    assert!(rows.iter().all(|row| {
        let payload = String::from_utf8_lossy(&row.payload);
        !payload.contains("secret-token") && !payload.contains("/api/webhooks/")
    }));

    let reply = fixture.submit("Reply body", Some(parent));
    let expected_reply = stable_delivery_event_id(
        &OrgId::from(ORG),
        &WebhookEvent::Feedback,
        &format!("feedback:{reply}"),
    );
    let rows = delivery_rows(&fixture.conn, "feedback");
    assert_eq!(rows.len(), 4);
    assert_eq!(
        rows.iter()
            .filter(|row| row.event_id == expected_reply)
            .count(),
        2,
        "a reply owns its own stable feedback subject"
    );
}

#[test]
fn viewer_and_publisher_resolve_each_transition_once_and_re_resolve_after_reopen() {
    let mut fixture = Fixture::new();
    let viewer_feedback = fixture.submit("viewer resolve", None);
    let publisher_feedback = fixture.submit("publisher resolve", None);
    fixture.subscribe("resolved", "resolved");
    let viewer = fixture.viewer();

    let first = feedback_delivery::resolve_as_viewer(
        &mut fixture.conn,
        &fixture.planning,
        PUBLIC_BASE,
        &fixture.meta,
        &viewer,
        viewer_feedback.clone(),
    )
    .expect("viewer resolve");
    assert!(first.changed);
    let retried = feedback_delivery::resolve_as_viewer(
        &mut fixture.conn,
        &fixture.planning,
        PUBLIC_BASE,
        &fixture.meta,
        &viewer,
        viewer_feedback,
    )
    .expect("viewer no-op");
    assert!(!retried.changed);
    assert_eq!(fixture.outbox_count(), 1);

    assert!(
        feedback_delivery::resolve_as_publisher(
            &mut fixture.conn,
            &fixture.planning,
            PUBLIC_BASE,
            &fixture.meta,
            publisher_feedback.clone(),
            "agent:publisher",
        )
        .expect("publisher resolve")
    );
    assert!(
        !feedback_delivery::resolve_as_publisher(
            &mut fixture.conn,
            &fixture.planning,
            PUBLIC_BASE,
            &fixture.meta,
            publisher_feedback.clone(),
            "agent:publisher",
        )
        .expect("publisher no-op")
    );
    assert_eq!(fixture.outbox_count(), 2);

    assert!(
        feedback_delivery::reopen_as_publisher(
            &mut fixture.conn,
            &fixture.meta,
            publisher_feedback.clone(),
        )
        .expect("reopen")
    );
    assert_eq!(fixture.outbox_count(), 2, "reopen remains local-only");
    assert!(
        feedback_delivery::resolve_as_publisher(
            &mut fixture.conn,
            &fixture.planning,
            PUBLIC_BASE,
            &fixture.meta,
            publisher_feedback,
            "agent:publisher",
        )
        .expect("resolve after reopen")
    );
    let rows = delivery_rows(&fixture.conn, "resolved");
    assert_eq!(rows.len(), 3);
    assert_eq!(
        rows.iter()
            .map(|row| row.event_id.as_str())
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        3,
        "each real resolution gets one stable event while no-op retries get none"
    );
}

#[test]
fn capacity_rejection_rolls_back_both_submit_and_resolve() {
    let mut fixture = Fixture::new();
    let existing = fixture.submit("existing", None);
    fixture.subscribe("feedback-resolved", "feedback,resolved");
    fill_tenant_capacity(&mut fixture.conn);
    assert_eq!(fixture.outbox_count(), 1_000);

    let submission = SubmitFeedback {
        viewer_email: EmailAddress::from("viewer@example.test"),
        body: "must roll back".to_owned(),
        parent_id: None,
        anchor: None,
        anchor_path: None,
        anchor_page: None,
    };
    assert_eq!(
        feedback_delivery::submit(
            &mut fixture.conn,
            &fixture.feedback_ids,
            &fixture.planning,
            PUBLIC_BASE,
            &fixture.meta,
            &submission,
            4_000,
        ),
        Err(AppError::RateLimited)
    );
    assert_eq!(fixture.feedback_count(), 1);

    let viewer = fixture.viewer();
    assert_eq!(
        feedback_delivery::resolve_as_viewer(
            &mut fixture.conn,
            &fixture.planning,
            PUBLIC_BASE,
            &fixture.meta,
            &viewer,
            existing.clone(),
        ),
        Err(AppError::RateLimited)
    );
    let unresolved = feedback::get(&fixture.conn, &existing)
        .expect("feedback")
        .expect("row");
    assert!(unresolved.resolved_at.is_none());
    assert_eq!(fixture.outbox_count(), 1_000);
}

#[test]
fn zero_subscribers_commit_feedback_without_provider_rows() {
    let mut fixture = Fixture::new();
    let id = fixture.submit("local discussion remains canonical", None);
    assert!(
        feedback::get(&fixture.conn, &id)
            .expect("feedback")
            .is_some()
    );
    assert_eq!(fixture.outbox_count(), 0);
}

#[test]
fn stale_tenant_or_ownership_scope_is_denied_inside_the_transaction() {
    let mut fixture = Fixture::new();
    let existing = fixture.submit("before move", None);
    fixture
        .conn
        .execute("INSERT OR IGNORE INTO orgs (name) VALUES ('globex')", [])
        .expect("target org");
    let tx = fixture
        .conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .expect("move transaction");
    tx.execute_batch("PRAGMA defer_foreign_keys=ON")
        .expect("defer");
    tx.execute("UPDATE artifacts SET org='globex' WHERE id=?1", [ARTIFACT])
        .expect("move artifact");
    tx.execute(
        "UPDATE feedback SET org='globex' WHERE id=?1",
        [&existing.0],
    )
    .expect("move feedback");
    tx.commit().expect("commit move");

    let submission = SubmitFeedback {
        viewer_email: EmailAddress::from("viewer@example.test"),
        body: "cross-tenant".to_owned(),
        parent_id: None,
        anchor: None,
        anchor_path: None,
        anchor_page: None,
    };
    assert_eq!(
        feedback_delivery::submit(
            &mut fixture.conn,
            &fixture.feedback_ids,
            &fixture.planning,
            PUBLIC_BASE,
            &fixture.meta,
            &submission,
            4_000,
        ),
        Err(AppError::NotFound("Not found".to_owned()))
    );
    let viewer = fixture.viewer();
    assert_eq!(
        feedback_delivery::delete_as_viewer(
            &mut fixture.conn,
            &fixture.meta,
            &viewer,
            existing.clone(),
        ),
        Err(AppError::NotFound("Not found".to_owned()))
    );
    assert_eq!(
        feedback_delivery::resolve_as_publisher(
            &mut fixture.conn,
            &fixture.planning,
            PUBLIC_BASE,
            &fixture.meta,
            existing.clone(),
            "agent:publisher",
        ),
        Err(AppError::NotFound("Not found".to_owned()))
    );
    assert!(
        feedback::get(&fixture.conn, &existing)
            .expect("feedback")
            .is_some()
    );
    assert_eq!(fixture.outbox_count(), 0);

    fixture.meta.org = OrgId::from("globex");
    fixture
        .conn
        .execute(
            "UPDATE artifacts SET client_id='replacement' WHERE id=?1",
            [ARTIFACT],
        )
        .expect("ownership change");
    assert_eq!(
        feedback_delivery::reopen_as_publisher(&mut fixture.conn, &fixture.meta, existing,),
        Err(AppError::NotFound("Not found".to_owned()))
    );
}

#[test]
fn delete_and_reopen_are_transactional_and_create_no_provider_policy_rows() {
    let mut fixture = Fixture::new();
    let id = fixture.submit("local-only mutations", None);
    feedback::resolve_as_publisher(&fixture.conn, &id, "agent:publisher").expect("seed resolved");
    fixture.subscribe("all-feedback", "feedback,resolved");

    assert!(
        feedback_delivery::reopen_as_publisher(&mut fixture.conn, &fixture.meta, id.clone())
            .expect("reopen")
    );
    assert!(
        !feedback_delivery::reopen_as_publisher(&mut fixture.conn, &fixture.meta, id.clone())
            .expect("no-op reopen")
    );
    assert_eq!(fixture.outbox_count(), 0);

    let viewer = fixture.viewer();
    let deleted =
        feedback_delivery::delete_as_viewer(&mut fixture.conn, &fixture.meta, &viewer, id.clone())
            .expect("delete");
    assert!(deleted.changed);
    assert!(
        feedback::get(&fixture.conn, &id)
            .expect("feedback")
            .is_none()
    );
    assert_eq!(fixture.outbox_count(), 0);
}

struct DeliveryRow {
    event_id: String,
    target: String,
    secret_ref: String,
    payload: Vec<u8>,
    digest: String,
}

fn delivery_rows(conn: &Connection, event_type: &str) -> Vec<DeliveryRow> {
    let mut statement = conn
        .prepare(
            "SELECT event_id, target_key, secret_ref, payload, payload_sha256 \
             FROM provider_delivery_outbox WHERE event_type=?1 ORDER BY created_at, id",
        )
        .expect("delivery query");
    statement
        .query_map([event_type], |row| {
            Ok(DeliveryRow {
                event_id: row.get(0)?,
                target: row.get(1)?,
                secret_ref: row.get(2)?,
                payload: row.get(3)?,
                digest: row.get(4)?,
            })
        })
        .expect("deliveries")
        .collect::<rusqlite::Result<Vec<_>>>()
        .expect("delivery rows")
}

fn fill_tenant_capacity(conn: &mut Connection) {
    let tx = conn.transaction().expect("capacity transaction");
    let rows = (0..1_000)
        .map(|index| {
            (
                EnqueueDelivery {
                    event_id: format!("capacity-event-{index}"),
                    tenant: ORG.to_owned(),
                    event_type: "published".to_owned(),
                    target_key: format!("capacity-target-{index}"),
                    secret_ref: format!("webhook:capacity-target-{index}"),
                    payload: b"{}".to_vec(),
                    payload_sha256: None,
                    durability_intent_id: None,
                    delivery_kind: outbox::DELIVERY_KIND_EVENT.to_owned(),
                    ordering_key: format!("capacity-target-{index}"),
                    depends_on_outbox_id: None,
                },
                format!("capacity-row-{index}"),
            )
        })
        .collect::<Vec<_>>();
    outbox::enqueue_many_in_transaction(&tx, &rows, 1_799_999_999_000).expect("fill capacity");
    tx.commit().expect("commit capacity");
}
