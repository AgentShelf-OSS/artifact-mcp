//! PBI-079 queue-contract tests: discussion ordering remains distinct from rate identities.

use artifact_mcp::{
    error::AppError,
    persistence::{
        migrations::{self, MigrationContext},
        outbox::{
            self, DELIVERY_KIND_DISCUSSION_MESSAGE, DELIVERY_KIND_DISCUSSION_THREAD,
            DELIVERY_KIND_EVENT, DeadLetterTransition, EnqueueDelivery,
        },
    },
};
use rusqlite::{Connection, TransactionBehavior};

fn db() -> Connection {
    let mut conn = Connection::open_in_memory().expect("db");
    conn.execute_batch("PRAGMA foreign_keys=ON").expect("fk");
    migrations::apply(&mut conn, &MigrationContext::empty()).expect("migrate");
    conn
}

fn event(event_id: &str, target: &str) -> EnqueueDelivery {
    EnqueueDelivery {
        event_id: event_id.into(),
        tenant: "acme".into(),
        event_type: "published".into(),
        target_key: target.into(),
        secret_ref: format!("webhook:{target}"),
        payload: br#"{\"content\":\"event\"}"#.to_vec(),
        payload_sha256: None,
        durability_intent_id: None,
        delivery_kind: DELIVERY_KIND_EVENT.into(),
        ordering_key: target.into(),
        depends_on_outbox_id: None,
    }
}

fn discussion(
    event_id: &str,
    kind: &str,
    ordering_key: &str,
    depends_on_outbox_id: Option<&str>,
) -> EnqueueDelivery {
    EnqueueDelivery {
        event_id: event_id.into(),
        tenant: "acme".into(),
        event_type: "discussion".into(),
        target_key: "connection-a".into(),
        secret_ref: "discussion:connection-a".into(),
        payload: br#"{\"version\":1}"#.to_vec(),
        payload_sha256: None,
        durability_intent_id: None,
        delivery_kind: kind.into(),
        ordering_key: ordering_key.into(),
        depends_on_outbox_id: depends_on_outbox_id.map(str::to_owned),
    }
}

fn enqueue(conn: &mut Connection, input: EnqueueDelivery, id: &str, now: i64) {
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .expect("tx");
    outbox::enqueue_in_transaction(&tx, &input, id, now).expect("enqueue");
    tx.commit().expect("commit");
}

#[test]
fn legacy_events_keep_target_fifo_as_their_ordering_key() {
    let mut conn = db();
    enqueue(&mut conn, event("legacy-first", "webhook-a"), "z-first", 1);
    enqueue(
        &mut conn,
        event("legacy-second", "webhook-a"),
        "a-second",
        1,
    );
    let row = outbox::claim_next(&mut conn, "worker", "token", 2)
        .expect("claim")
        .expect("row");
    assert_eq!(row.id, "z-first", "insertion order wins over lexical ID");
    assert_eq!(row.delivery_kind, DELIVERY_KIND_EVENT);
    assert_eq!(row.ordering_key, "webhook-a");
    assert_eq!(row.depends_on_outbox_id, None);
    assert!(
        outbox::accepted(
            &conn,
            &row.id,
            "worker",
            "token",
            row.lease_version,
            "message",
            2,
        )
        .expect("accept first")
    );
    assert_eq!(
        outbox::claim_next(&mut conn, "worker", "second-token", 2)
            .expect("claim")
            .expect("second")
            .id,
        "a-second"
    );
}

#[test]
fn same_millisecond_discussion_messages_follow_insertion_order_not_random_ids() {
    let mut conn = db();
    let ordering_key = "discussion:artifact-fifo:1";
    enqueue(
        &mut conn,
        discussion(
            "fifo-root-event",
            DELIVERY_KIND_DISCUSSION_THREAD,
            ordering_key,
            None,
        ),
        "fifo-root",
        100,
    );
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .expect("message transaction");
    outbox::enqueue_many_in_transaction(
        &tx,
        &[
            (
                discussion(
                    "fifo-first-event",
                    DELIVERY_KIND_DISCUSSION_MESSAGE,
                    ordering_key,
                    Some("fifo-root"),
                ),
                "z-first-message".into(),
            ),
            (
                discussion(
                    "fifo-second-event",
                    DELIVERY_KIND_DISCUSSION_MESSAGE,
                    ordering_key,
                    Some("fifo-root"),
                ),
                "a-second-message".into(),
            ),
        ],
        100,
    )
    .expect("enqueue same-millisecond messages");
    tx.commit().expect("commit messages");
    let root = outbox::claim_next(&mut conn, "worker", "root-token", 100)
        .expect("claim")
        .expect("root");
    assert_eq!(root.id, "fifo-root");
    assert!(
        outbox::accepted(
            &conn,
            &root.id,
            "worker",
            "root-token",
            root.lease_version,
            "root-message",
            100,
        )
        .expect("accept root")
    );
    let first = outbox::claim_next(&mut conn, "worker", "first-token", 100)
        .expect("claim")
        .expect("first message");
    assert_eq!(first.id, "z-first-message");
    assert!(
        outbox::accepted(
            &conn,
            &first.id,
            "worker",
            "first-token",
            first.lease_version,
            "first-message",
            100,
        )
        .expect("accept first")
    );
    assert_eq!(
        outbox::claim_next(&mut conn, "worker", "second-token", 100)
            .expect("claim")
            .expect("second message")
            .id,
        "a-second-message"
    );
    assert_eq!(
        conn.query_row(
            "SELECT group_concat(id, ',') FROM (SELECT id FROM provider_delivery_outbox \
             WHERE ordering_key = ?1 ORDER BY created_at, id)",
            [ordering_key],
            |row| row.get::<_, String>(0),
        )
        .expect("durable FIFO order"),
        "fifo-root,z-first-message,a-second-message"
    );
}

#[test]
fn distinct_discussion_lanes_can_claim_concurrently_on_one_connection() {
    let mut conn = db();
    enqueue(
        &mut conn,
        discussion(
            "root-a",
            DELIVERY_KIND_DISCUSSION_THREAD,
            "discussion:artifact-a:1",
            None,
        ),
        "root-a",
        1,
    );
    enqueue(
        &mut conn,
        discussion(
            "root-b",
            DELIVERY_KIND_DISCUSSION_THREAD,
            "discussion:artifact-b:1",
            None,
        ),
        "root-b",
        2,
    );
    let first = outbox::claim_next(&mut conn, "one", "token-one", 3)
        .expect("claim")
        .expect("first");
    let second = outbox::claim_next(&mut conn, "two", "token-two", 3)
        .expect("claim")
        .expect("second");
    assert_ne!(first.id, second.id);
    assert_eq!(first.target_key, second.target_key, "one rate identity");
    assert_ne!(
        first.ordering_key, second.ordering_key,
        "independent FIFO lanes"
    );
}

#[test]
fn discussion_bucket_rate_limit_reloads_with_its_opaque_connection_reference() {
    let mut conn = db();
    enqueue(
        &mut conn,
        discussion(
            "root-rate-limit",
            DELIVERY_KIND_DISCUSSION_THREAD,
            "discussion:artifact-a:1",
            None,
        ),
        "root-rate-limit",
        1,
    );
    outbox::persist_rate_limit(
        &mut conn,
        "bucket",
        "connection-a",
        "discussion-bucket-a",
        "discussion:connection-a",
        500,
        100,
    )
    .expect("persist discussion bucket");
    assert_eq!(
        conn.query_row(
            "SELECT bucket_id, top_level_secret_ref FROM provider_delivery_rate_limits \
             WHERE scope = 'bucket'",
            [],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .expect("stored bucket"),
        (
            "discussion-bucket-a".into(),
            "discussion:connection-a".into()
        )
    );
    assert!(
        outbox::claim_next(&mut conn, "worker", "early", 499)
            .expect("rate limited")
            .is_none()
    );
    let claimed = outbox::claim_next(&mut conn, "worker", "after", 500)
        .expect("claim")
        .expect("row");
    assert_eq!(claimed.bucket_id, "discussion-bucket-a");
    assert_eq!(claimed.secret_ref, "discussion:connection-a");
    assert!(matches!(
        outbox::persist_rate_limit(
            &mut conn,
            "bucket",
            "connection-a",
            "unsafe-bucket",
            "https://discord.com/api/webhooks/1/raw-secret",
            500,
            100,
        ),
        Err(AppError::Validation(_))
    ));
}

#[test]
fn root_dependency_gates_and_terminal_failure_surfaces_on_descendants() {
    let mut conn = db();
    enqueue(
        &mut conn,
        discussion(
            "root",
            DELIVERY_KIND_DISCUSSION_THREAD,
            "discussion:artifact-a:1",
            None,
        ),
        "root",
        1,
    );
    enqueue(
        &mut conn,
        discussion(
            "reply",
            DELIVERY_KIND_DISCUSSION_MESSAGE,
            "discussion:artifact-a:1",
            Some("root"),
        ),
        "reply",
        2,
    );
    let root = outbox::claim_next(&mut conn, "worker", "root-token", 3)
        .expect("claim")
        .expect("root");
    assert!(
        outbox::claim_next(&mut conn, "other", "other-token", 3)
            .expect("gated")
            .is_none()
    );
    assert!(
        outbox::dead_letter(
            &conn,
            &root.id,
            "worker",
            "root-token",
            root.lease_version,
            DeadLetterTransition {
                classification: "validation_failed".into(),
                error: "validation_failed".into(),
                duplicate_risk: false,
            },
            4,
        )
        .expect("terminal")
    );
    assert!(
        outbox::claim_next(&mut conn, "other", "other-token", 5)
            .expect("propagate")
            .is_none()
    );
    let result: (String, String) = conn
        .query_row(
            "SELECT state, result_classification FROM provider_delivery_outbox WHERE id = 'reply'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("dependent");
    assert_eq!(result, ("dead_letter".into(), "dependency_failed".into()));
}

#[test]
fn discussion_contract_rejects_secret_target_dependency_and_idempotency_mismatches() {
    let mut conn = db();
    let root = discussion(
        "root",
        DELIVERY_KIND_DISCUSSION_THREAD,
        "discussion:artifact-a:1",
        None,
    );
    enqueue(&mut conn, root.clone(), "root", 1);
    assert_eq!(
        conn.query_row(
            "SELECT target_key, secret_ref FROM provider_delivery_outbox WHERE id = 'root'",
            [],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .expect("exact connection contract"),
        ("connection-a".into(), "discussion:connection-a".into())
    );
    let tx = conn.transaction().expect("tx");
    assert_eq!(
        outbox::enqueue_in_transaction(&tx, &root, "other", 2)
            .expect("idempotent")
            .id,
        "root"
    );
    drop(tx);

    let bad_secret = EnqueueDelivery {
        secret_ref: "discussion:connection-b".into(),
        ..root.clone()
    };
    let prefixed_target = EnqueueDelivery {
        target_key: "discussion:connection-a".into(),
        secret_ref: "discussion:discussion-connection-a".into(),
        ..root.clone()
    };
    let child_without_root = discussion(
        "child",
        DELIVERY_KIND_DISCUSSION_MESSAGE,
        "discussion:artifact-a:1",
        None,
    );
    let root_with_dependency = discussion(
        "root-dependency",
        DELIVERY_KIND_DISCUSSION_THREAD,
        "discussion:artifact-a:2",
        Some("root"),
    );
    let child_with_unknown_root = discussion(
        "unknown-root",
        DELIVERY_KIND_DISCUSSION_MESSAGE,
        "discussion:artifact-a:1",
        Some("not-a-root"),
    );
    enqueue(
        &mut conn,
        discussion(
            "intermediate",
            DELIVERY_KIND_DISCUSSION_MESSAGE,
            "discussion:artifact-a:1",
            Some("root"),
        ),
        "intermediate",
        2,
    );
    let child_with_message_predecessor = discussion(
        "message-predecessor",
        DELIVERY_KIND_DISCUSSION_MESSAGE,
        "discussion:artifact-a:1",
        Some("intermediate"),
    );
    for (input, id) in [
        (bad_secret, "bad-secret"),
        (prefixed_target, "prefixed-target"),
        (child_without_root, "bad-child"),
        (root_with_dependency, "bad-root"),
        (child_with_unknown_root, "unknown-root"),
        (child_with_message_predecessor, "message-predecessor"),
    ] {
        let tx = conn.transaction().expect("tx");
        assert!(matches!(
            outbox::enqueue_in_transaction(&tx, &input, id, 2),
            Err(AppError::Validation(_))
        ));
        drop(tx);
    }
    let changed_ordering = EnqueueDelivery {
        ordering_key: "discussion:artifact-b:1".into(),
        ..root
    };
    let tx = conn.transaction().expect("tx");
    assert_eq!(
        outbox::enqueue_in_transaction(&tx, &changed_ordering, "changed", 2)
            .expect_err("idempotency conflict"),
        AppError::Conflict("outbox idempotency conflict".into())
    );
}
