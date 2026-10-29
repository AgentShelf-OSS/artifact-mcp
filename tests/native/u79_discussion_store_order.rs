//! Characterization of order-sensitive behaviour in the Discord discussion store: which check
//! answers first when several would fail, and which row a multi-row lookup returns.

use std::sync::Arc;

use artifact_mcp::{
    error::AppError,
    model::{ArtifactId, EmailAddress, FeedbackId, OrgId, Viewer},
    persistence::{
        db::{self, Database, DbPool},
        discussions::{
            CreateDiscussionConnection, CreateDiscussionMessageLink, DiscussionMode,
            DiscussionState, DiscussionStore,
        },
    },
    security::{audit::MutationAudit, crypto::WebhookUrlProtection},
};

use crate::u03_support::{TempDataDir, scalar};

async fn count(pool: &DbPool, sql: &'static str) -> i64 {
    db::interact(pool, move |conn| Ok(scalar::<i64>(conn, sql)))
        .await
        .expect("count")
}

const AUDIT_KEY: [u8; 32] = [0x7a; 32];

fn store(label: &str) -> (TempDataDir, DbPool, DiscussionStore) {
    let dir = TempDataDir::new(label);
    let pool = Database::open_at(dir.path()).expect("migrate database");
    let store = DiscussionStore::new(pool.clone(), Arc::new(WebhookUrlProtection::Plaintext));
    (dir, pool, store)
}

fn admin_audit() -> MutationAudit {
    MutationAudit::viewer(&Viewer {
        email: Some(EmailAddress("admin@example.test".to_owned())),
        org: Some(OrgId("admin".to_owned())),
        is_admin: true,
    })
    .expect("admin audit")
}

async fn seed(pool: &DbPool, feedback: &[&str]) {
    let feedback: Vec<String> = feedback.iter().map(|id| (*id).to_owned()).collect();
    db::interact(pool, move |conn| {
        conn.execute("INSERT INTO orgs (name) VALUES ('acme')", [])
            .expect("org");
        conn.execute(
            "INSERT INTO artifacts (id, client_id, org, title) VALUES ('artifact-a', 'publisher', 'acme', 'Artifact')",
            [],
        )
        .expect("artifact");
        for id in feedback {
            conn.execute(
                "INSERT INTO feedback (id, artifact_id, org, viewer_email, body, artifact_revision) VALUES (?1, 'artifact-a', 'acme', 'viewer@example.test', 'comment', 1)",
                [&id],
            )
            .expect("feedback");
        }
        Ok(())
    })
    .await
    .expect("seed");
}

async fn outbox(pool: &DbPool, id: &str) {
    let id = id.to_owned();
    db::interact(pool, move |conn| {
        conn.execute(
            "INSERT INTO provider_delivery_outbox (id, provider, event_id, tenant, event_type, target_key, bucket_id, secret_ref, payload, payload_sha256, state, next_attempt_at, created_at, updated_at) VALUES (?1, 'discord', ?1, 'acme', 'discussion', 'connection', 'connection', 'discussion:connection', x'7B7D', '44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a', 'ready', 0, 0, 0)",
            [&id],
        )
        .expect("outbox");
        Ok(())
    })
    .await
    .expect("outbox seed");
}

async fn connect(store: &DiscussionStore) -> String {
    store
        .upsert_connection(CreateDiscussionConnection {
            org: "acme".into(),
            url: "https://discord.com/api/webhooks/123456/secret".into(),
            label: "Forum".into(),
        })
        .await
        .expect("connection")
        .id
}

fn link_input(
    connection_id: &str,
    feedback: &str,
    outbox: &str,
    generation: u64,
) -> CreateDiscussionMessageLink {
    CreateDiscussionMessageLink {
        artifact_id: "artifact-a".into(),
        org: "acme".into(),
        connection_id: connection_id.to_owned(),
        feedback_id: feedback.into(),
        delivery_event_id: outbox.to_owned(),
        outbox_id: outbox.to_owned(),
        external_thread_id: None,
        generation,
    }
}

#[tokio::test]
async fn feedback_link_lookup_returns_the_highest_generation_whatever_the_insert_order() {
    let (_dir, pool, store) = store("u79-link-newest-generation");
    seed(&pool, &["feedback-a"]).await;
    let connection = connect(&store).await;
    for outbox_id in ["outbox-g2", "outbox-g1", "outbox-g3"] {
        outbox(&pool, outbox_id).await;
    }
    // Generation 3 is inserted between 2 and 1 in rowid order; neither rowid nor outbox id
    // order may decide the answer.
    for (outbox_id, generation) in [("outbox-g2", 2), ("outbox-g3", 3), ("outbox-g1", 1)] {
        store
            .create_message_link(link_input(&connection, "feedback-a", outbox_id, generation))
            .await
            .expect("link");
    }
    let newest = store
        .message_link_for_feedback(
            &ArtifactId::from("artifact-a"),
            &OrgId::from("acme"),
            &FeedbackId::from("feedback-a"),
        )
        .await
        .expect("lookup")
        .expect("link exists");
    assert_eq!(newest.generation, 3);
    assert_eq!(newest.outbox_id, "outbox-g3");
}

#[tokio::test]
async fn create_message_link_checks_generation_before_identity_fields() {
    let (_dir, _pool, store) = store("u79-link-validation-order");
    let mut input = link_input("", "", "", 0);
    input.artifact_id = ArtifactId::from("");
    // Generation 0 and empty identity fields are both invalid; the generation check answers first.
    let error = store
        .create_message_link(input.clone())
        .await
        .expect_err("invalid");
    assert!(
        matches!(&error, AppError::Validation(message) if message == "invalid discussion generation"),
        "{error:?}"
    );
    input.generation = 1;
    let error = store.create_message_link(input).await.expect_err("invalid");
    assert!(
        matches!(&error, AppError::Validation(message) if message == "discussion identity fields are required"),
        "{error:?}"
    );
}

#[tokio::test]
async fn create_message_link_replay_keeps_the_original_and_conflicting_replay_is_rejected() {
    let (_dir, pool, store) = store("u79-link-replay");
    seed(&pool, &["feedback-a"]).await;
    let connection = connect(&store).await;
    outbox(&pool, "outbox-a").await;
    let first = store
        .create_message_link(link_input(&connection, "feedback-a", "outbox-a", 1))
        .await
        .expect("first");
    let replay = store
        .create_message_link(link_input(&connection, "feedback-a", "outbox-a", 1))
        .await
        .expect("identical replay");
    assert_eq!(first, replay);
    let mut other_generation = link_input(&connection, "feedback-a", "outbox-a", 2);
    other_generation.delivery_event_id = "outbox-a".to_owned();
    let error = store
        .create_message_link(other_generation)
        .await
        .expect_err("same outbox, other generation");
    assert!(
        matches!(&error, AppError::Conflict(message) if message == "discussion delivery idempotency conflict"),
        "{error:?}"
    );
    let mut other_event = link_input(&connection, "feedback-a", "outbox-a", 1);
    other_event.delivery_event_id = "different-event".to_owned();
    let error = store
        .create_message_link(other_event)
        .await
        .expect_err("same outbox, other event");
    assert!(matches!(error, AppError::Conflict(_)), "{error:?}");
}

#[tokio::test]
async fn disable_mirror_without_a_row_is_local_only_and_never_creates_one() {
    let (_dir, pool, store) = store("u79-disable-without-row");
    seed(&pool, &[]).await;
    let local = store
        .disable_mirror(&ArtifactId::from("artifact-a"), &OrgId::from("acme"))
        .await
        .expect("existing artifact without a row");
    assert_eq!(local.mode, DiscussionMode::ArtifactMcpOnly);
    assert_eq!(local.state, DiscussionState::Local);
    assert_eq!(local.generation, 0);
    assert!(local.created_at.is_none());
    let rows = count(&pool, "SELECT COUNT(*) FROM artifact_discussions").await;
    assert_eq!(rows, 0, "disabling must not backfill a row");
    let error = store
        .disable_mirror(&ArtifactId::from("missing"), &OrgId::from("acme"))
        .await
        .expect_err("unknown artifact");
    assert!(matches!(error, AppError::NotFound(_)), "{error:?}");
    let error = store
        .disable_mirror(&ArtifactId::from("artifact-a"), &OrgId::from("other"))
        .await
        .expect_err("artifact in another org");
    assert!(matches!(error, AppError::NotFound(_)), "{error:?}");
}

#[tokio::test]
async fn retry_only_mints_a_generation_for_a_failed_mirror() {
    let (_dir, pool, store) = store("u79-retry-states");
    seed(&pool, &[]).await;
    // No row at all: the not-enabled conflict answers before any connection check.
    let error = store
        .retry_audited(
            ArtifactId::from("artifact-a"),
            OrgId::from("acme"),
            "owner@example.test".to_owned(),
            admin_audit(),
            AUDIT_KEY,
        )
        .await
        .expect_err("not enabled");
    assert!(
        matches!(&error, AppError::Conflict(message) if message == "Discord discussion mirroring is not enabled."),
        "{error:?}"
    );
    connect(&store).await;
    let enabled = store
        .enable_mirror(
            &ArtifactId::from("artifact-a"),
            &OrgId::from("acme"),
            "owner",
        )
        .await
        .expect("enable");
    assert_eq!(enabled.generation, 1);
    // Pending is not failed: the current row comes back unchanged and nothing is audited.
    let audits_before = count(&pool, "SELECT COUNT(*) FROM security_audit_events").await;
    let unchanged = store
        .retry_audited(
            ArtifactId::from("artifact-a"),
            OrgId::from("acme"),
            "owner@example.test".to_owned(),
            admin_audit(),
            AUDIT_KEY,
        )
        .await
        .expect("pending retry");
    assert_eq!(unchanged.generation, 1);
    assert_eq!(unchanged.state, DiscussionState::Pending);
    assert_eq!(
        count(&pool, "SELECT COUNT(*) FROM security_audit_events").await,
        audits_before
    );

    db::interact(&pool, move |conn| {
        conn.execute(
            "UPDATE artifact_discussions SET state = 'failed', thread_id = 'thread', last_error = 'boom'",
            [],
        )
        .expect("fail");
        Ok(())
    })
    .await
    .expect("fail update");
    let retried = store
        .retry_audited(
            ArtifactId::from("artifact-a"),
            OrgId::from("acme"),
            "owner@example.test".to_owned(),
            admin_audit(),
            AUDIT_KEY,
        )
        .await
        .expect("failed retry");
    assert_eq!(retried.generation, 2);
    assert_eq!(retried.state, DiscussionState::Pending);
    assert!(retried.thread_id.is_none());
    assert!(retried.last_error.is_none());
    assert_eq!(
        count(&pool, "SELECT COUNT(*) FROM security_audit_events").await,
        audits_before + 1
    );
}
