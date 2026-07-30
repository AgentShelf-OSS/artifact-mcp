//! PBI-056 Stage C: lifecycle/outbox crash and rollback proofs.
//!
//! These tests deliberately use U08's production failpoint harness.  The filesystem mutation
//! and its SQLite intent/outbox rows therefore follow the exact production ordering; we only
//! inspect durable state between the interrupted request and startup reconciliation.

use std::sync::Arc;

use artifact_mcp::{
    artifacts::lifecycle::{ArtifactStore, FaultPoint, ScriptedFaults},
    config::{SequentialIdSource, StorageLimits},
    error::AppError,
    persistence::outbox::{self, EnqueueDelivery},
    ports::ArtifactService as _,
};
use rusqlite::params;

use crate::u08_support::{Fixture, TEST_AUDIT_KEY, html_update, mutation_audit};

const OLD: &str = "<h1>old</h1>";
const NEW: &str = "<h1>new</h1>";
const THIRD: &str = "<h1>third</h1>";

fn subscribe(fixture: &Fixture, id: &str) {
    let conn = artifact_mcp::persistence::db::checkout(&fixture.pool).expect("checkout");
    conn.execute(
        "INSERT INTO org_webhooks (id, org, url, events, created_at) \
         VALUES (?1, 'acme', 'https://discord.com/api/webhooks/123/token', \
                 'published,updated,restored,deleted', '2026-07-30 00:00:00')",
        params![id],
    )
    .expect("subscriber");
}

fn faulted_store(fixture: &Fixture, faults: Arc<ScriptedFaults>) -> ArtifactStore {
    ArtifactStore::with_faults_for_test(
        fixture.pool.clone(),
        fixture.artifact_dir.clone(),
        StorageLimits::default(),
        Arc::new(SequentialIdSource::default()),
        faults,
        TEST_AUDIT_KEY,
    )
}

fn delivery_state(fixture: &Fixture, event_type: &str) -> (String, Option<String>) {
    let conn = artifact_mcp::persistence::db::checkout(&fixture.pool).expect("checkout");
    conn.query_row(
        "SELECT state, durability_intent_id FROM provider_delivery_outbox \
         WHERE event_type = ?1",
        [event_type],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )
    .expect("delivery row")
}

fn assert_blocked_and_unclaimable(fixture: &Fixture, event_type: &str) {
    let (state, intent) = delivery_state(fixture, event_type);
    assert_eq!(
        state, "blocked",
        "{event_type} delivery stays durability-gated"
    );
    assert!(
        intent.is_some(),
        "{event_type} row keeps its intent binding"
    );

    let mut conn = artifact_mcp::persistence::db::checkout(&fixture.pool).expect("checkout");
    assert!(
        outbox::claim_next(&mut conn, "crash-proof", "lease", 1_800_000_000_000)
            .expect("claim scan")
            .is_none(),
        "a blocked {event_type} row is never sent before reconciliation"
    );
}

async fn assert_released_once(fixture: &Fixture, event_type: &str) {
    fixture
        .store
        .audit_storage(true)
        .await
        .expect("startup reconciliation");
    assert_eq!(
        delivery_state(fixture, event_type),
        ("ready".to_owned(), None)
    );
    assert_eq!(
        fixture.count(&format!(
            "SELECT COUNT(*) FROM provider_delivery_outbox WHERE event_type = '{event_type}'"
        )),
        1,
        "reconciliation releases the one event that the mutation committed"
    );
    assert_eq!(
        fixture.count("SELECT COUNT(*) FROM artifact_durability_intents"),
        0
    );

    fixture
        .store
        .audit_storage(true)
        .await
        .expect("idempotent restart reconciliation");
    assert_eq!(
        fixture.count(&format!(
            "SELECT COUNT(*) FROM provider_delivery_outbox WHERE event_type = '{event_type}'"
        )),
        1,
        "a second startup never creates a duplicate {event_type} event"
    );
}

#[tokio::test]
async fn publish_crash_holds_delivery_until_startup_proves_the_body_durable() {
    let faults = Arc::new(ScriptedFaults::new().crash_once(FaultPoint::PublishComplete));
    let fixture = Fixture::with_faults("u56-publish-outbox-crash", faults.clone());
    subscribe(&fixture, "publish-subscriber");

    fixture
        .try_publish(artifact_mcp::model::ArtifactContent::SingleHtml(
            OLD.to_owned(),
        ))
        .await
        .expect_err("simulated process death");
    assert!(faults.all_fired());
    assert_blocked_and_unclaimable(&fixture, "published");

    assert_released_once(&fixture, "published").await;
}

#[tokio::test]
async fn update_crash_holds_delivery_until_startup_installs_the_committed_body() {
    let fixture = Fixture::new("u56-update-outbox-crash");
    let published = fixture.publish_single(OLD).await;
    subscribe(&fixture, "update-subscriber");
    let faults = Arc::new(ScriptedFaults::new().crash_once(FaultPoint::UpdateSnapshot));
    let store = faulted_store(&fixture, faults.clone());

    store
        .update_for(&published, html_update(1, NEW), mutation_audit())
        .await
        .expect_err("simulated process death");
    assert!(faults.all_fired());
    assert_blocked_and_unclaimable(&fixture, "updated");

    assert_released_once(&fixture, "updated").await;
    let recovered = fixture.reload(&published).expect("committed update");
    assert_eq!(recovered.revision, 2);
    assert_eq!(fixture.body_on_disk(&recovered).as_deref(), Some(NEW));
}

#[tokio::test]
async fn restore_crash_holds_delivery_until_startup_installs_the_restored_body() {
    let fixture = Fixture::new("u56-restore-outbox-crash");
    let published = fixture.publish_single(OLD).await;
    let updated = fixture
        .store
        .update_for(&published, html_update(1, NEW), mutation_audit())
        .await
        .expect("create restorable revision")
        .meta;
    subscribe(&fixture, "restore-subscriber");
    let faults = Arc::new(ScriptedFaults::new().crash_once(FaultPoint::UpdateSnapshot));
    let store = faulted_store(&fixture, faults.clone());

    store
        .restore_for(&updated, 1, None, mutation_audit())
        .await
        .expect_err("simulated process death");
    assert!(faults.all_fired());
    assert_blocked_and_unclaimable(&fixture, "restored");

    assert_released_once(&fixture, "restored").await;
    let recovered = fixture.reload(&published).expect("committed restore");
    assert_eq!(recovered.revision, 3);
    assert_eq!(fixture.body_on_disk(&recovered).as_deref(), Some(OLD));
}

#[tokio::test]
async fn delete_crash_holds_delivery_until_startup_proves_cleanup_complete() {
    let fixture = Fixture::new("u56-delete-outbox-crash");
    let published = fixture.publish_single(OLD).await;
    subscribe(&fixture, "delete-subscriber");
    let faults = Arc::new(ScriptedFaults::new().crash_once(FaultPoint::DeleteHistoryRemove));
    let store = faulted_store(&fixture, faults.clone());

    store
        .delete_for(&published, mutation_audit())
        .await
        .expect_err("simulated process death");
    assert!(faults.all_fired());
    assert_blocked_and_unclaimable(&fixture, "deleted");

    assert_released_once(&fixture, "deleted").await;
    assert!(fixture.reload(&published).is_none());
}

#[tokio::test]
async fn reconciliation_compensates_reverted_metadata_and_retains_ambiguous_evidence() {
    let fixture = Fixture::new("u56-outbox-reverted-and-ambiguous");
    let published = fixture.publish_single(OLD).await;
    subscribe(&fixture, "reconcile-subscriber");
    let faults = Arc::new(ScriptedFaults::new().crash_once(FaultPoint::UpdateSnapshot));
    let store = faulted_store(&fixture, faults.clone());

    store
        .update_for(&published, html_update(1, NEW), mutation_audit())
        .await
        .expect_err("simulated process death");
    assert_blocked_and_unclaimable(&fixture, "updated");

    // Model a completed, authoritative metadata rollback while preserving the real staged-body
    // crash evidence. This is a database state transition, not a hand-built filesystem state.
    fixture.execute(&format!(
        "UPDATE artifacts SET body_sha256 = '{}', revision = 1 WHERE id = '{}'",
        artifact_mcp::artifacts::digest::sha256_hex(OLD.as_bytes()),
        published.id.0
    ));
    fixture
        .store
        .audit_storage(true)
        .await
        .expect("reconcile proven rollback");
    assert_eq!(
        fixture.count("SELECT COUNT(*) FROM provider_delivery_outbox WHERE event_type = 'updated'"),
        0,
        "a proven rollback compensates its blocked delivery"
    );
    assert_eq!(
        fixture.count("SELECT COUNT(*) FROM artifact_durability_intents"),
        0
    );

    let ambiguous = Fixture::new("u56-outbox-ambiguous");
    let published = ambiguous.publish_single(OLD).await;
    subscribe(&ambiguous, "ambiguous-subscriber");
    let faults = Arc::new(ScriptedFaults::new().crash_once(FaultPoint::UpdateSnapshot));
    faulted_store(&ambiguous, faults)
        .update_for(&published, html_update(1, NEW), mutation_audit())
        .await
        .expect_err("simulated process death");
    ambiguous.execute(&format!(
        "UPDATE artifacts SET body_sha256 = '{}', revision = 3 WHERE id = '{}'",
        artifact_mcp::artifacts::digest::sha256_hex(THIRD.as_bytes()),
        published.id.0
    ));
    ambiguous
        .store
        .audit_storage(true)
        .await
        .expect("ambiguous evidence is inspected");
    assert_blocked_and_unclaimable(&ambiguous, "updated");
    assert_eq!(
        ambiguous.count("SELECT COUNT(*) FROM artifact_durability_intents"),
        1,
        "ambiguous evidence remains concealed for an operator rather than being sent"
    );
}

fn fill_tenant_capacity(fixture: &Fixture) {
    let mut conn = artifact_mcp::persistence::db::checkout(&fixture.pool).expect("checkout");
    let tx = conn.transaction().expect("fill transaction");
    let rows = (0..1_000)
        .map(|number| {
            (
                EnqueueDelivery {
                    event_id: format!("fill-{number}"),
                    tenant: "acme".to_owned(),
                    event_type: "published".to_owned(),
                    target_key: format!("fill-target-{number}"),
                    secret_ref: format!("webhook:fill-target-{number}"),
                    payload: b"{}".to_vec(),
                    payload_sha256: None,
                    durability_intent_id: None,
                    delivery_kind: outbox::DELIVERY_KIND_EVENT.to_owned(),
                    ordering_key: format!("fill-target-{number}"),
                    depends_on_outbox_id: None,
                },
                format!("fill-row-{number}"),
            )
        })
        .collect::<Vec<_>>();
    outbox::enqueue_many_in_transaction(&tx, &rows, 1_800_000_000_000).expect("fill capacity");
    tx.commit().expect("commit capacity fill");
}

#[tokio::test]
async fn fanout_capacity_rejection_rolls_back_publish_and_update_cleanup() {
    let publish = Fixture::new("u56-publish-capacity-rollback");
    subscribe(&publish, "publish-capacity");
    fill_tenant_capacity(&publish);
    assert_eq!(
        publish
            .try_publish(artifact_mcp::model::ArtifactContent::SingleHtml(
                NEW.to_owned()
            ))
            .await
            .expect_err("queue capacity rejects publish"),
        AppError::RateLimited
    );
    assert_eq!(publish.count("SELECT COUNT(*) FROM artifacts"), 0);
    assert_eq!(
        publish.count("SELECT COUNT(*) FROM artifact_durability_intents"),
        0
    );
    assert_eq!(
        publish.count("SELECT COUNT(*) FROM provider_delivery_outbox"),
        1_000
    );
    assert!(publish.staging_entries().is_empty());

    let update = Fixture::new("u56-update-capacity-rollback");
    let existing = update.publish_single(OLD).await;
    subscribe(&update, "update-capacity");
    fill_tenant_capacity(&update);
    assert_eq!(
        update
            .store
            .update_for(&existing, html_update(1, NEW), mutation_audit())
            .await
            .expect_err("queue capacity rejects update"),
        AppError::RateLimited
    );
    assert_eq!(update.reload(&existing).expect("original row").revision, 1);
    assert_eq!(update.body_on_disk(&existing).as_deref(), Some(OLD));
    assert_eq!(
        update.count("SELECT COUNT(*) FROM artifact_durability_intents"),
        0
    );
    assert_eq!(
        update.count("SELECT COUNT(*) FROM provider_delivery_outbox"),
        1_000
    );
    assert!(update.staging_entries().is_empty());
}
