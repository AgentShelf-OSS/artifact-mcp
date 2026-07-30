//! PBI-056 Stage C: lifecycle mutations plan durable, intent-gated Discord delivery.

use std::sync::{Arc, Mutex};

use artifact_mcp::security::access::AccessPolicy;
use artifact_mcp::{
    artifacts::lifecycle::{ArtifactStore, PostCommitPreviewScheduler},
    model::{ArtifactContent, ArtifactMeta},
    ports::ArtifactService as _,
};
use rusqlite::params;

use crate::u08_support::{Fixture, html_update, mutation_audit, publish_request, publisher};

#[derive(Debug, Default)]
struct RecordingPreviewScheduler(Mutex<Vec<ArtifactMeta>>);

impl RecordingPreviewScheduler {
    fn scheduled(&self) -> Vec<ArtifactMeta> {
        self.0.lock().expect("scheduler lock").clone()
    }
}

impl PostCommitPreviewScheduler for RecordingPreviewScheduler {
    fn schedule(&self, _: ArtifactStore, meta: ArtifactMeta) {
        self.0.lock().expect("scheduler lock").push(meta);
    }
}

fn subscribe(fixture: &Fixture, id: &str) {
    let conn = artifact_mcp::persistence::db::checkout(&fixture.pool).expect("checkout");
    conn.execute(
        "INSERT INTO org_webhooks (id, org, url, events, created_at) VALUES (?1, 'acme', 'https://discord.com/api/webhooks/123/token', 'published,updated,restored,deleted', '2026-01-01 00:00:00')",
        params![id],
    )
    .expect("subscriber");
}

#[tokio::test]
async fn lifecycle_delivery_is_released_only_after_each_durable_mutation() {
    let fixture = Fixture::new("u56-lifecycle-cutover");
    subscribe(&fixture, "wh-one");
    let published = fixture.publish_single("<h1>published</h1>").await;

    let updated = fixture
        .store
        .update(
            AccessPolicy::authorize_publisher_write(
                &publisher(),
                Some(published.clone()),
                &published.id.0,
                "unused",
            )
            .expect("write")
            .into_authorized(),
            html_update(published.revision, "<h1>updated</h1>"),
            mutation_audit(),
        )
        .await
        .expect("update")
        .meta;
    let restored = fixture
        .store
        .restore(
            AccessPolicy::authorize_publisher_write(
                &publisher(),
                Some(updated.clone()),
                &updated.id.0,
                "unused",
            )
            .expect("write")
            .into_authorized(),
            1,
            None,
            mutation_audit(),
        )
        .await
        .expect("restore")
        .meta;
    assert!(
        fixture
            .store
            .delete(
                AccessPolicy::authorize_publisher_delete(
                    &publisher(),
                    Some(restored),
                    &published.id.0,
                )
                .expect("delete")
                .into_authorized(),
                mutation_audit(),
            )
            .await
            .expect("delete")
    );

    let conn = artifact_mcp::persistence::db::checkout(&fixture.pool).expect("checkout");
    let rows = conn
        .prepare(
            "SELECT event_type, state, durability_intent_id, payload FROM provider_delivery_outbox ORDER BY id",
        )
        .expect("query")
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Vec<u8>>(3)?,
            ))
        })
        .expect("rows")
        .collect::<rusqlite::Result<Vec<_>>>()
        .expect("collect");
    assert_eq!(rows.len(), 4);
    assert_eq!(
        rows.iter().map(|row| row.0.as_str()).collect::<Vec<_>>(),
        ["published", "updated", "restored", "deleted"]
    );
    assert!(
        rows.iter()
            .all(|(_, state, intent, _)| state == "ready" && intent.is_none())
    );
    assert!(rows.iter().all(|(_, _, _, payload)| {
        let payload = String::from_utf8_lossy(payload);
        payload.contains(r#""provider":"discord""#) && !payload.contains("discord.com/api/webhooks")
    }));
    assert_eq!(
        fixture.count("SELECT COUNT(*) FROM artifact_durability_intents"),
        0
    );
}

#[tokio::test]
async fn zero_subscribers_still_schedule_each_gallery_thumbnail_without_legacy_delivery() {
    let fixture = Fixture::new("u56-gallery-thumbnail-post-commit");
    let scheduler = Arc::new(RecordingPreviewScheduler::default());
    let store = fixture
        .store
        .clone()
        .with_post_commit_preview_scheduler(scheduler.clone());

    let published = store
        .publish(
            publish_request(ArtifactContent::SingleHtml("<h1>published</h1>".into())),
            mutation_audit(),
        )
        .await
        .expect("publish")
        .meta;
    let updated = store
        .update(
            AccessPolicy::authorize_publisher_write(
                &publisher(),
                Some(published.clone()),
                &published.id.0,
                "unused",
            )
            .expect("write")
            .into_authorized(),
            html_update(published.revision, "<h1>updated</h1>"),
            mutation_audit(),
        )
        .await
        .expect("update")
        .meta;
    store
        .restore(
            AccessPolicy::authorize_publisher_write(
                &publisher(),
                Some(updated.clone()),
                &updated.id.0,
                "unused",
            )
            .expect("write")
            .into_authorized(),
            1,
            None,
            mutation_audit(),
        )
        .await
        .expect("restore");

    let scheduled = scheduler.scheduled();
    assert_eq!(scheduled.len(), 3);
    assert_eq!(
        scheduled
            .iter()
            .map(|meta| meta.revision)
            .collect::<Vec<_>>(),
        [1, 2, 3]
    );
    // No subscriber means durable fanout creates no provider rows. The scheduler takes no
    // NotificationSink, so it cannot reintroduce detached legacy Discord sends.
    assert_eq!(
        fixture.count("SELECT COUNT(*) FROM provider_delivery_outbox"),
        0
    );
}
