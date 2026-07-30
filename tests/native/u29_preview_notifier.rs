//! PBI-065: production preview-notifier admission coverage.
//!
//! These tests exercise the library adapter used by `main.rs` with its actual bounded queue and
//! Discord notifier. The source only observes the deferred body-loader boundary; it does not
//! duplicate queue admission or fallback-delivery logic.

use std::{
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::Duration,
};

use artifact_mcp::{
    config::PreviewConfig,
    integrations::{
        notify::DiscordNotifier,
        preview::PreviewRenderer,
        preview_notifier::{ArtifactPreviewNotifier, PreviewArtifactSource},
        thumbnails::{
            DEFAULT_MAX_PNG_BYTES, PreviewHtml, PreviewIntegration, ThumbnailQueue, ThumbnailStore,
        },
    },
    model::{ArtifactId, ArtifactMeta, ClientId, CreateWebhook, OrgId, Timestamp, WebhookEvent},
    ports::{BoxFuture, NotificationSink, integrations::PreviewPriority},
};
use tokio::sync::Notify;

use crate::u12_support::{RecordingTransport, fixture, notifier as discord_notifier, payload};

const DIGEST: &str = "cafebabe00112233445566778899aabbccddeeff00112233445566778899aabb";

#[derive(Clone)]
struct ObservedArtifactSource {
    meta: ArtifactMeta,
    queue: Arc<ThumbnailQueue>,
    body_loads: Arc<AtomicUsize>,
    reserved_bytes_on_load: Arc<AtomicU64>,
    body_started: Arc<Notify>,
    body_release: Option<Arc<Notify>>,
}

impl ObservedArtifactSource {
    fn new(meta: ArtifactMeta, queue: Arc<ThumbnailQueue>) -> Self {
        Self {
            meta,
            queue,
            body_loads: Arc::new(AtomicUsize::new(0)),
            reserved_bytes_on_load: Arc::new(AtomicU64::new(u64::MAX)),
            body_started: Arc::new(Notify::new()),
            body_release: None,
        }
    }

    fn blocking(mut self) -> Self {
        self.body_release = Some(Arc::new(Notify::new()));
        self
    }
}

impl PreviewArtifactSource for ObservedArtifactSource {
    fn find_meta<'a>(&'a self, id: &'a ArtifactId) -> BoxFuture<'a, Option<ArtifactMeta>> {
        Box::pin(async move { (id == &self.meta.id).then(|| self.meta.clone()) })
    }

    fn deferred_body(&self, _meta: &ArtifactMeta) -> PreviewHtml {
        let queue = Arc::clone(&self.queue);
        let body_loads = Arc::clone(&self.body_loads);
        let reserved_bytes_on_load = Arc::clone(&self.reserved_bytes_on_load);
        let body_started = Arc::clone(&self.body_started);
        let body_release = self.body_release.clone();
        PreviewHtml::Deferred(Box::new(move || {
            Box::pin(async move {
                body_loads.fetch_add(1, Ordering::SeqCst);
                reserved_bytes_on_load.store(queue.depth().reserved_bytes, Ordering::SeqCst);
                body_started.notify_one();
                if let Some(release) = body_release {
                    release.notified().await;
                }
                Some("<p>deferred artifact body</p>".to_owned())
            })
        }))
    }
}

fn meta(id: &str, bytes: u64) -> ArtifactMeta {
    ArtifactMeta {
        id: ArtifactId(id.to_owned()),
        client_id: ClientId("client".to_owned()),
        org: OrgId("acme".to_owned()),
        title: "Report".to_owned(),
        description: String::new(),
        bytes,
        created_at: Timestamp("2026-01-01 00:00:00".to_owned()),
        updated_at: Timestamp("2026-01-01 00:00:00".to_owned()),
        uploader_label: "Publisher".to_owned(),
        owner_email: None,
        is_bundle: false,
        entry: String::new(),
        revision: 3,
        category: String::new(),
        hidden: false,
        body_sha256: DIGEST.to_owned(),
    }
}

fn previews(data_dir: &Path, max_jobs: usize, max_bytes: u64) -> Arc<PreviewIntegration> {
    let renderer = Arc::new(PreviewRenderer::new(&PreviewConfig::default()));
    let store = Arc::new(ThumbnailStore::new(
        data_dir,
        DEFAULT_MAX_PNG_BYTES,
        renderer,
    ));
    let queue =
        ThumbnailQueue::new_with_limits_and_counter(store.clone(), max_jobs, max_bytes, None);
    Arc::new(PreviewIntegration::from_parts(store, queue))
}

fn blocker(started: Arc<Notify>, release: Arc<Notify>) -> PreviewHtml {
    PreviewHtml::Deferred(Box::new(move || {
        Box::pin(async move {
            started.notify_one();
            release.notified().await;
            Some("<p>queue blocker</p>".to_owned())
        })
    }))
}

async fn notifier_with_webhook(
    label: &str,
    previews: Arc<PreviewIntegration>,
    source: ObservedArtifactSource,
) -> (
    ArtifactPreviewNotifier,
    Arc<RecordingTransport>,
    crate::u12_support::TempDir,
) {
    let (dir, _pool, webhooks) = fixture(label, "acme", None).await;
    webhooks
        .create(CreateWebhook {
            org: OrgId("acme".to_owned()),
            url: "https://discord.com/api/webhooks/123456789012345678/test-token".to_owned(),
            label: String::new(),
            events: Some(vec![WebhookEvent::Published]),
        })
        .await
        .expect("create subscribed webhook");
    let transport = RecordingTransport::ok();
    let discord: Arc<DiscordNotifier> =
        Arc::new(discord_notifier(&webhooks, Arc::clone(&transport)));
    (
        ArtifactPreviewNotifier::new(Arc::new(source), previews, discord),
        transport,
        dir,
    )
}

async fn wait_for_delivery(transport: &RecordingTransport) {
    tokio::time::timeout(Duration::from_secs(1), async {
        while transport.started() == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("fallback webhook is delivered");
}

#[tokio::test]
async fn job_count_rejection_skips_the_deferred_body_loader_and_preview_worker() {
    let (data, _pool, _webhooks) = fixture("notifier-job-capacity", "acme", None).await;
    let previews = previews(data.path(), 1, 1_024);
    let queue = Arc::clone(previews.queue());
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let active = queue
        .try_enqueue(
            meta("active000001", 1),
            blocker(Arc::clone(&started), Arc::clone(&release)),
            PreviewPriority::High,
        )
        .expect("active job admitted");
    started.notified().await;
    let queued = queue
        .try_enqueue(
            meta("queued000001", 1),
            PreviewHtml::Deferred(Box::new(|| Box::pin(async { Some(String::new()) }))),
            PreviewPriority::High,
        )
        .expect("one waiting job admitted");

    let source = ObservedArtifactSource::new(meta("abc123def456", 1), Arc::clone(&queue));
    let (notifier, transport, _webhook_data) = notifier_with_webhook(
        "notifier-job-fallback",
        Arc::clone(&previews),
        source.clone(),
    )
    .await;

    assert_eq!(
        notifier
            .emit(WebhookEvent::Published, OrgId("acme".to_owned()), payload())
            .await,
        Ok(()),
        "preview pressure must not fail the mutation"
    );
    assert_eq!(source.body_loads.load(Ordering::SeqCst), 0);
    assert_eq!(queue.depth().high, 1, "rejection added no preview job");
    wait_for_delivery(&transport).await;
    let calls = transport.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].1.content_type, "application/json");

    release.notify_one();
    assert_eq!(active.await, None);
    assert_eq!(queued.await, None);
    assert_eq!(
        source.body_loads.load(Ordering::SeqCst),
        0,
        "the rejected notifier job never created a worker that could invoke its loader"
    );
}

#[tokio::test]
async fn byte_capacity_rejection_skips_the_deferred_body_loader_and_falls_back_to_discord() {
    let (data, _pool, _webhooks) = fixture("notifier-byte-capacity", "acme", None).await;
    let previews = previews(data.path(), 2, 42);
    let queue = Arc::clone(previews.queue());
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let active = queue
        .try_enqueue(
            meta("active000002", 42),
            blocker(Arc::clone(&started), Arc::clone(&release)),
            PreviewPriority::High,
        )
        .expect("active job admitted");
    started.notified().await;
    assert_eq!(queue.depth().reserved_bytes, 42);

    let source = ObservedArtifactSource::new(meta("abc123def456", 1), Arc::clone(&queue));
    let (notifier, transport, _webhook_data) = notifier_with_webhook(
        "notifier-byte-fallback",
        Arc::clone(&previews),
        source.clone(),
    )
    .await;

    assert_eq!(
        notifier
            .emit(WebhookEvent::Published, OrgId("acme".to_owned()), payload())
            .await,
        Ok(())
    );
    assert_eq!(source.body_loads.load(Ordering::SeqCst), 0);
    assert_eq!(
        queue.depth().reserved_bytes,
        42,
        "rejected bytes were not reserved"
    );
    wait_for_delivery(&transport).await;
    let calls = transport.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].1.content_type, "application/json");

    release.notify_one();
    assert_eq!(active.await, None);
    assert_eq!(source.body_loads.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn accepted_notifier_work_reserves_declared_bytes_before_loading_the_body() {
    let (data, _pool, _webhooks) = fixture("notifier-admission-order", "acme", None).await;
    let previews = previews(data.path(), 2, 64);
    let queue = Arc::clone(previews.queue());
    let source =
        ObservedArtifactSource::new(meta("abc123def456", 42), Arc::clone(&queue)).blocking();
    let (notifier, transport, _webhook_data) = notifier_with_webhook(
        "notifier-admission-delivery",
        Arc::clone(&previews),
        source.clone(),
    )
    .await;

    assert_eq!(
        notifier
            .emit(WebhookEvent::Published, OrgId("acme".to_owned()), payload())
            .await,
        Ok(())
    );
    source.body_started.notified().await;
    assert_eq!(source.body_loads.load(Ordering::SeqCst), 1);
    assert_eq!(
        source.reserved_bytes_on_load.load(Ordering::SeqCst),
        42,
        "the queue reserves the declared body bytes before starting the deferred loader"
    );
    assert_eq!(queue.depth().reserved_bytes, 42);

    source
        .body_release
        .expect("blocking source has a release gate")
        .notify_one();
    wait_for_delivery(&transport).await;
}
