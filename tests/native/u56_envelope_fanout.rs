//! PBI-056 Stage B: canonical envelopes and subscriber-only atomic fanout.
use artifact_mcp::{
    error::AppError,
    integrations::delivery_envelope::{DeliveryEnvelopeV1, stable_delivery_event_id},
    model::{ArtifactId, NotificationPayload, OrgId, WebhookEvent},
    persistence::{
        migrations::{self, MigrationContext},
        outbox::{self, EnqueueDelivery},
        outbox_fanout,
    },
};
use rusqlite::{Connection, TransactionBehavior};

fn db() -> Connection {
    let mut conn = Connection::open_in_memory().expect("db");
    conn.execute_batch("PRAGMA foreign_keys=ON").expect("fk");
    migrations::apply(&mut conn, &MigrationContext::empty()).expect("migrate");
    conn.execute("INSERT INTO orgs (name) VALUES ('acme')", [])
        .expect("org");
    conn
}

#[test]
fn fanout_capacity_refusal_leaves_the_subscriber_batch_unwritten() {
    let mut conn = db();
    webhook(&conn, "wh-cap", "published");
    let tx = conn.transaction().expect("tx");
    let inputs = (0..1000)
        .map(|n| {
            (
                EnqueueDelivery {
                    event_id: format!("fill-{n}"),
                    tenant: "acme".into(),
                    event_type: "published".into(),
                    target_key: format!("target-{n}"),
                    secret_ref: format!("webhook:target-{n}"),
                    payload: b"{}".to_vec(),
                    payload_sha256: None,
                    durability_intent_id: None,
                    delivery_kind: outbox::DELIVERY_KIND_EVENT.to_owned(),
                    ordering_key: format!("target-{n}"),
                    depends_on_outbox_id: None,
                },
                format!("fill-{n}"),
            )
        })
        .collect::<Vec<_>>();
    outbox::enqueue_many_in_transaction(&tx, &inputs, 0).expect("fill");
    tx.commit().expect("commit");
    let tx = conn.transaction().expect("tx");
    assert_eq!(
        outbox_fanout::fanout_in_transaction(
            &tx,
            &envelope(),
            &OrgId("acme".into()),
            &WebhookEvent::Published,
            None,
            1,
            || Ok("fanout-cap".into())
        )
        .expect_err("full"),
        AppError::RateLimited
    );
    drop(tx);
    assert_eq!(
        conn.query_row::<i64, _, _>("SELECT COUNT(*) FROM provider_delivery_outbox", [], |row| {
            row.get(0)
        })
        .expect("count"),
        1000
    );
}
fn payload() -> NotificationPayload {
    NotificationPayload {
        artifact_id: ArtifactId("artifact-1".into()),
        title: "Report".into(),
        url: "".into(),
        description: "A report".into(),
        uploader_label: "Ada".into(),
        category: "Docs".into(),
        revision: 2,
        bytes: 128,
        viewer_email: None,
        body: None,
        resolver: None,
    }
}
fn envelope() -> DeliveryEnvelopeV1 {
    let org = OrgId("acme".into());
    let event = WebhookEvent::Published;
    let event_id = stable_delivery_event_id(&org, &event, "artifact:artifact-1:2");
    DeliveryEnvelopeV1::build(event_id, &org, &event, &payload()).expect("envelope")
}
fn webhook(conn: &Connection, id: &str, events: &str) {
    conn.execute("INSERT INTO org_webhooks (id,org,url,events,created_at) VALUES (?1,'acme','https://discord.com/api/webhooks/123/testing-token',?2,'2026-01-01 00:00:00')",[id,events]).expect("webhook");
}

#[test]
fn canonical_envelope_is_stable_hashed_and_never_contains_preview_or_webhook_secret() {
    let first = envelope();
    let second = envelope();
    assert_eq!(
        first.event_id(),
        "delivery:v1:7bcad03fcf3d78ad6ec5d6dbe903dead096b9d437bb8b91c00d47a0c94e4dba4"
    );
    assert_eq!(first.event_id(), second.event_id());
    assert_eq!(
        first.canonical_bytes().expect("bytes"),
        second.canonical_bytes().expect("bytes")
    );
    assert_eq!(first.payload_sha256().expect("hash").len(), 64);
    let bytes = String::from_utf8(first.canonical_bytes().expect("bytes")).expect("json");
    assert!(bytes.contains("\"version\":1"));
    assert!(!bytes.contains("preview.png"));
    assert!(!bytes.contains("discord.com/api/webhooks"));
    assert!(!bytes.contains("testing-token"));
    assert_eq!(
        bytes,
        r#"{"version":1,"event_id":"delivery:v1:7bcad03fcf3d78ad6ec5d6dbe903dead096b9d437bb8b91c00d47a0c94e4dba4","tenant":"acme","event_type":"published","provider":"discord","payload":{"embeds":[{"color":3120756,"author":{"name":"acme"},"title":"Report","fields":[{"name":"Publisher","value":"Ada","inline":true},{"name":"Category","value":"Docs","inline":true},{"name":"Revision","value":"2","inline":true},{"name":"Size","value":"128 B","inline":true}],"description":"A report"}]}}"#
    );
}

#[test]
fn strict_envelope_decode_is_bound_and_discord_body_excludes_envelope_metadata() {
    let envelope = envelope();
    let bytes = envelope.canonical_bytes().expect("bytes");
    let decoded = DeliveryEnvelopeV1::decode_canonical(
        &bytes,
        &OrgId("acme".into()),
        &WebhookEvent::Published,
        envelope.event_id(),
        Some(&envelope.payload_sha256().expect("hash")),
    )
    .expect("strict decode");
    let body =
        String::from_utf8(decoded.discord_request_body_bytes().expect("body")).expect("json");
    assert_eq!(
        body,
        r#"{"embeds":[{"color":3120756,"author":{"name":"acme"},"title":"Report","fields":[{"name":"Publisher","value":"Ada","inline":true},{"name":"Category","value":"Docs","inline":true},{"name":"Revision","value":"2","inline":true},{"name":"Size","value":"128 B","inline":true}],"description":"A report"}]}"#
    );
    assert!(body.starts_with(r#"{"embeds":"#));
    assert!(!body.contains(r#""version""#));
    assert!(!body.contains(r#""event_id""#));
    assert!(!body.contains(r#""tenant""#));
    assert!(!body.contains(r#""provider""#));
}

#[test]
fn tampered_or_mismatched_envelopes_cannot_enter_fanout() {
    let mut conn = db();
    webhook(&conn, "wh-one", "published");
    let original = envelope();
    let hash = original.payload_sha256().expect("hash");
    let altered = String::from_utf8(original.canonical_bytes().expect("bytes"))
        .expect("json")
        .replace("Report", "Altered");
    assert!(
        DeliveryEnvelopeV1::decode_canonical(
            altered.as_bytes(),
            &OrgId("acme".into()),
            &WebhookEvent::Published,
            original.event_id(),
            Some(&hash),
        )
        .is_err()
    );
    assert!(
        DeliveryEnvelopeV1::decode_canonical(
            br#"[]"#,
            &OrgId("acme".into()),
            &WebhookEvent::Published,
            original.event_id(),
            None,
        )
        .is_err()
    );
    let unknown_field = format!(
        "{}{}",
        String::from_utf8(original.canonical_bytes().expect("bytes"))
            .expect("json")
            .trim_end_matches('}'),
        r#","unexpected":true}"#
    );
    assert!(
        DeliveryEnvelopeV1::decode_canonical(
            unknown_field.as_bytes(),
            &OrgId("acme".into()),
            &WebhookEvent::Published,
            original.event_id(),
            None,
        )
        .is_err()
    );
    assert!(
        DeliveryEnvelopeV1::decode_canonical(
            &original.canonical_bytes().expect("bytes"),
            &OrgId("other".into()),
            &WebhookEvent::Published,
            original.event_id(),
            None,
        )
        .is_err()
    );
    let canonical = String::from_utf8(original.canonical_bytes().expect("bytes")).expect("json");
    let reordered_top_level = canonical.replacen(
        r#"{"version":1,"event_id":"delivery:"#,
        r#"{"event_id":"delivery:"#,
        1,
    );
    let reordered_top_level = reordered_top_level.replacen(
        r#"","tenant":"acme""#,
        r#"","version":1,"tenant":"acme""#,
        1,
    );
    assert!(
        DeliveryEnvelopeV1::decode_canonical(
            reordered_top_level.as_bytes(),
            &OrgId("acme".into()),
            &WebhookEvent::Published,
            original.event_id(),
            None,
        )
        .is_err()
    );
    let reordered_nested = canonical.replacen(
        r#"{"color":3120756,"author":{"name":"acme"},"title":"Report""#,
        r#"{"title":"Report","color":3120756,"author":{"name":"acme"}"#,
        1,
    );
    assert!(
        DeliveryEnvelopeV1::decode_canonical(
            reordered_nested.as_bytes(),
            &OrgId("acme".into()),
            &WebhookEvent::Published,
            original.event_id(),
            None,
        )
        .is_err()
    );
    let deleted = DeliveryEnvelopeV1::build(
        stable_delivery_event_id(&OrgId("acme".into()), &WebhookEvent::Deleted, "artifact-1"),
        &OrgId("acme".into()),
        &WebhookEvent::Deleted,
        &payload(),
    )
    .expect("deleted envelope");
    let tx = conn.transaction().expect("tx");
    assert!(
        outbox_fanout::fanout_in_transaction(
            &tx,
            &deleted,
            &OrgId("acme".into()),
            &WebhookEvent::Published,
            None,
            1,
            || Ok("forged".into()),
        )
        .is_err()
    );
    drop(tx);
    assert_eq!(
        conn.query_row::<i64, _, _>("SELECT COUNT(*) FROM provider_delivery_outbox", [], |row| {
            row.get(0)
        })
        .expect("count"),
        0
    );
}
#[test]
fn fanout_selects_only_subscribers_and_never_reads_webhook_urls() {
    let mut conn = db();
    webhook(&conn, "wh-one", "published,updated");
    webhook(&conn, "wh-two", "published");
    webhook(&conn, "wh-other", "feedback");
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .expect("tx");
    let rows = outbox_fanout::fanout_in_transaction(
        &tx,
        &envelope(),
        &OrgId("acme".into()),
        &WebhookEvent::Published,
        None,
        100,
        {
            let mut n = 0;
            move || {
                n += 1;
                Ok(format!("outbox-{n}"))
            }
        },
    )
    .expect("fanout");
    assert_eq!(rows.len(), 2);
    assert_eq!(
        rows.iter()
            .map(|row| row.target_key.as_str())
            .collect::<Vec<_>>(),
        ["wh-one", "wh-two"]
    );
    assert!(
        rows.iter()
            .all(|row| row.secret_ref.starts_with("webhook:wh-"))
    );
    assert!(
        rows.iter()
            .all(|row| !String::from_utf8_lossy(&row.payload).contains("webhooks"))
    );
    tx.commit().expect("commit");
}
#[test]
fn zero_subscribers_and_tampered_subscriber_ids_leave_no_partial_fanout() {
    let mut conn = db();
    let tx = conn.transaction().expect("tx");
    assert!(
        outbox_fanout::fanout_in_transaction(
            &tx,
            &envelope(),
            &OrgId("acme".into()),
            &WebhookEvent::Published,
            None,
            0,
            || Ok("outbox".into())
        )
        .expect("zero")
        .is_empty()
    );
    tx.commit().expect("commit");
    webhook(&conn, "wh-good", "published");
    webhook(&conn, "bad/id", "published");
    let tx = conn.transaction().expect("tx");
    assert!(
        outbox_fanout::fanout_in_transaction(
            &tx,
            &envelope(),
            &OrgId("acme".into()),
            &WebhookEvent::Published,
            None,
            1,
            {
                let mut n = 0;
                move || {
                    n += 1;
                    Ok(format!("outbox-{n}"))
                }
            }
        )
        .is_err()
    );
    drop(tx);
    assert_eq!(
        conn.query_row::<i64, _, _>("SELECT COUNT(*) FROM provider_delivery_outbox", [], |row| {
            row.get(0)
        })
        .expect("none"),
        0
    );
}
