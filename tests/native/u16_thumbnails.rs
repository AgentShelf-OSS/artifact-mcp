//! U16: digest-addressed thumbnail persistence, cleanup, the serial priority lane, and the
//! optional-by-design guarantee.
//!
//! The single hardest requirement in this unit is **optionality** (rebuild blueprint risk #11):
//! the preview sidecar runs under an optional compose profile and is usually absent, so a preview
//! failure must never fail an otherwise valid publish, update or restore. The
//! `never_fails_the_caller` group below drives the frozen [`PreviewService`] port through every
//! failure mode the sidecar and the filesystem can produce and asserts `Ok(None)` each time —
//! `Err` is never constructed on any path.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use artifact_mcp::artifacts::paths::{BodyDigest, SafeArtifactId, thumbnail_path};
use artifact_mcp::config::{AppConfig, PreviewConfig, SeededRandom};
use artifact_mcp::http::ingress::IngressState;
use artifact_mcp::integrations::preview::PreviewRenderer;
use artifact_mcp::integrations::thumbnails::{
    DEFAULT_MAX_PNG_BYTES, PreviewArtifactIndex, PreviewArtifactRef, PreviewHtml,
    PreviewIntegration, ThumbnailQueue, ThumbnailStore, valid_png,
};
use artifact_mcp::model::{ArtifactId, ArtifactMeta, ClientId, OrgId, Timestamp};
use artifact_mcp::ports::PreviewService;
use artifact_mcp::ports::integrations::PreviewPriority;

use crate::u03_support::TempDataDir;
use crate::u16_support::{StubRenderer, StubReply, png_of, sample_png};

const ID: &str = "abc123def456";
const OTHER_ID: &str = "zzz999yyy888";
const DIGEST: &str = "cafebabe00112233445566778899aabbccddeeff00112233445566778899aabb";
const OTHER_DIGEST: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn meta(id: &str, digest: &str, is_bundle: bool) -> ArtifactMeta {
    ArtifactMeta {
        id: ArtifactId(id.to_owned()),
        client_id: ClientId("client".to_owned()),
        org: OrgId("acme".to_owned()),
        title: "Report".to_owned(),
        description: String::new(),
        bytes: 42,
        created_at: Timestamp("2026-01-01 00:00:00".to_owned()),
        updated_at: Timestamp("2026-01-01 00:00:00".to_owned()),
        uploader_label: "Publisher".to_owned(),
        owner_email: None,
        is_bundle,
        entry: String::new(),
        revision: 3,
        category: String::new(),
        hidden: false,
        body_sha256: digest.to_owned(),
    }
}

fn single(id: &str, digest: &str) -> ArtifactMeta {
    meta(id, digest, false)
}

/// A store wired to `stub`, with deterministic temp-file entropy.
fn thumbnail_store(data_dir: &Path, config: PreviewConfig) -> Arc<ThumbnailStore> {
    let renderer = Arc::new(PreviewRenderer::new(&config));
    Arc::new(
        ThumbnailStore::new(data_dir, config.max_png_bytes, renderer)
            .with_random(Arc::new(SeededRandom::new(7))),
    )
}

fn expected_path(data_dir: &Path, id: &str, digest: &str) -> PathBuf {
    thumbnail_path(
        data_dir,
        &SafeArtifactId::parse(id).expect("valid id"),
        &BodyDigest::parse(digest).expect("valid digest"),
    )
}

/// Entry names of a directory, sorted; empty when it does not exist.
fn entries(dir: &Path) -> Vec<String> {
    let Ok(read) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = read
        .flatten()
        .filter_map(|entry| entry.file_name().into_string().ok())
        .collect();
    names.sort();
    names
}

// ---------------------------------------------------------------------------
// Digest-addressed, atomic persistence
// ---------------------------------------------------------------------------

#[tokio::test]
async fn persists_at_the_digest_addressed_path_and_leaves_no_temp_file() {
    let data = TempDataDir::new("u16-persist");
    let png = sample_png();
    let stub = StubRenderer::rendering(png.clone());
    let store = thumbnail_store(data.path(), stub.config());

    let produced = store
        .ensure_thumbnail(&single(ID, DIGEST), Some("<p>x</p>"))
        .await;
    assert_eq!(produced, Some(png.clone()));

    let target = expected_path(data.path(), ID, DIGEST);
    assert_eq!(
        target,
        data.path()
            .join("previews")
            .join(ID)
            .join(format!("{DIGEST}.png"))
    );
    assert_eq!(std::fs::read(&target).expect("thumbnail on disk"), png);

    // The temporary file is renamed, never left behind: exactly one entry, the final name.
    assert_eq!(
        entries(&data.path().join("previews").join(ID)),
        vec![format!("{DIGEST}.png")]
    );
}

#[tokio::test]
async fn an_existing_thumbnail_is_served_without_rendering_again() {
    let data = TempDataDir::new("u16-existing");
    let png = sample_png();
    let stub = StubRenderer::rendering(png.clone());
    let store = thumbnail_store(data.path(), stub.config());
    let meta = single(ID, DIGEST);

    assert!(
        store
            .ensure_thumbnail(&meta, Some("<p>x</p>"))
            .await
            .is_some()
    );
    assert_eq!(stub.request_count(), 1);

    // A second store shares the directory but has a cold render cache: the hit must come from
    // disk, not from the in-process cache.
    let cold = thumbnail_store(data.path(), stub.config());
    assert_eq!(
        cold.ensure_thumbnail(&meta, Some("<p>x</p>")).await,
        Some(png)
    );
    assert_eq!(
        stub.request_count(),
        1,
        "an on-disk thumbnail was re-rendered"
    );
}

#[tokio::test]
async fn durable_delivery_can_read_a_prior_digest_after_a_same_tenant_rapid_update() {
    let data = TempDataDir::new("u16-durable-prior-digest");
    let png = sample_png();
    let stub = StubRenderer::rendering(png.clone());
    let store = thumbnail_store(data.path(), stub.config());
    let prior = single(ID, DIGEST);
    assert_eq!(
        store.ensure_thumbnail(&prior, Some("<p>prior</p>")).await,
        Some(png.clone())
    );

    // The live metadata has already advanced. Browser thumbnail reads correctly reject the
    // stale digest, while a claimed durable event may still attach the cache entry that its
    // immutable envelope referenced.
    let current = single(ID, OTHER_DIGEST);
    assert_eq!(store.read_thumbnail(&current, DIGEST).await, None);
    assert_eq!(
        store.read_delivery_thumbnail(&current.id, DIGEST).await,
        Some(png)
    );
}

#[tokio::test]
async fn a_persistence_failure_yields_no_thumbnail_and_no_debris() {
    // A plain file where the artifact's preview directory belongs: `create_dir_all` fails, and
    // it fails for root too, so the assertion holds regardless of who runs the suite.
    let data = TempDataDir::new("u16-unwritable");
    std::fs::create_dir_all(data.path().join("previews")).expect("previews dir");
    std::fs::write(data.path().join("previews").join(ID), b"not a directory").expect("blocker");

    let stub = StubRenderer::rendering(sample_png());
    let store = thumbnail_store(data.path(), stub.config());

    assert_eq!(
        store
            .ensure_thumbnail(&single(ID, DIGEST), Some("<p>x</p>"))
            .await,
        None,
        "bytes that are not on disk must not be reported as a thumbnail"
    );
    assert_eq!(stub.request_count(), 1, "the render itself did happen");
    assert_eq!(
        std::fs::read(data.path().join("previews").join(ID)).expect("blocker survives"),
        b"not a directory"
    );
    assert_eq!(entries(&data.path().join("previews")), vec![ID.to_owned()]);
}

#[tokio::test]
async fn an_oversized_render_is_never_written() {
    let data = TempDataDir::new("u16-oversized");
    let stub = StubRenderer::rendering(png_of(4_096));
    let store = thumbnail_store(
        data.path(),
        PreviewConfig {
            max_png_bytes: 1_024,
            ..stub.config()
        },
    );

    assert_eq!(
        store
            .ensure_thumbnail(&single(ID, DIGEST), Some("<p>x</p>"))
            .await,
        None
    );
    assert!(entries(&data.path().join("previews").join(ID)).is_empty());
}

#[tokio::test]
async fn an_invalid_png_is_never_written() {
    let data = TempDataDir::new("u16-invalid");
    let stub = StubRenderer::rendering(b"<html>not a png</html>".to_vec());
    let store = thumbnail_store(data.path(), stub.config());

    assert_eq!(
        store
            .ensure_thumbnail(&single(ID, DIGEST), Some("<p>x</p>"))
            .await,
        None
    );
    assert!(entries(&data.path().join("previews").join(ID)).is_empty());
}

// ---------------------------------------------------------------------------
// Path safety
// ---------------------------------------------------------------------------

#[tokio::test]
async fn only_validated_ids_and_digests_can_form_a_path() {
    let data = TempDataDir::new("u16-path-safety");
    let stub = StubRenderer::rendering(sample_png());
    let store = thumbnail_store(data.path(), stub.config());

    for (id, digest) in [
        ("../../etc", DIGEST),
        ("ABC123DEF456", DIGEST),
        ("short", DIGEST),
        (ID, "not-a-digest"),
        (
            ID,
            "CAFEBABE00112233445566778899AABBCCDDEEFF00112233445566778899AABB",
        ),
        (ID, ""),
    ] {
        assert_eq!(
            store
                .ensure_thumbnail(&single(id, digest), Some("<p>x</p>"))
                .await,
            None,
            "id {id:?} digest {digest:?} was accepted"
        );
    }
    assert_eq!(
        stub.request_count(),
        0,
        "an unvalidated id reached the renderer"
    );
    assert!(entries(&data.path().join("previews")).is_empty());
}

#[tokio::test]
async fn bundles_never_get_a_thumbnail() {
    let data = TempDataDir::new("u16-bundle");
    let stub = StubRenderer::rendering(sample_png());
    let store = thumbnail_store(data.path(), stub.config());
    let bundle = meta(ID, DIGEST, true);

    assert_eq!(
        store.ensure_thumbnail(&bundle, Some("<p>x</p>")).await,
        None
    );
    assert_eq!(store.read_thumbnail(&bundle, DIGEST).await, None);
    assert_eq!(stub.request_count(), 0);
}

// ---------------------------------------------------------------------------
// Reads
// ---------------------------------------------------------------------------

#[tokio::test]
async fn only_the_current_digest_is_served() {
    let data = TempDataDir::new("u16-stale-digest");
    let png = sample_png();
    let stub = StubRenderer::rendering(png.clone());
    let store = thumbnail_store(data.path(), stub.config());
    let meta = single(ID, DIGEST);
    assert!(
        store
            .ensure_thumbnail(&meta, Some("<p>x</p>"))
            .await
            .is_some()
    );

    assert_eq!(store.read_thumbnail(&meta, DIGEST).await, Some(png));
    assert_eq!(
        store.read_thumbnail(&meta, OTHER_DIGEST).await,
        None,
        "a stale ?v= must fall through to the placeholder"
    );
}

#[tokio::test]
async fn a_corrupt_thumbnail_is_deleted_on_read() {
    let data = TempDataDir::new("u16-corrupt");
    let target = expected_path(data.path(), ID, DIGEST);
    std::fs::create_dir_all(target.parent().expect("parent")).expect("preview dir");
    std::fs::write(&target, b"truncated").expect("corrupt file");

    let store = thumbnail_store(data.path(), PreviewConfig::default());
    assert_eq!(
        store.read_thumbnail(&single(ID, DIGEST), DIGEST).await,
        None
    );
    assert!(!target.exists(), "the invalid PNG was left in place");
}

// ---------------------------------------------------------------------------
// Cleanup
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_new_revision_sweeps_the_previous_one_and_stray_temp_files() {
    let data = TempDataDir::new("u16-cleanup");
    let dir = data.path().join("previews").join(ID);
    std::fs::create_dir_all(&dir).expect("preview dir");
    std::fs::write(dir.join(format!("{OTHER_DIGEST}.png")), sample_png()).expect("old revision");
    std::fs::write(dir.join(format!(".{DIGEST}.deadbeef.tmp")), b"interrupted").expect("temp");
    std::fs::create_dir_all(dir.join("stray-directory")).expect("stray");

    let stub = StubRenderer::rendering(sample_png());
    let store = thumbnail_store(data.path(), stub.config());
    assert!(
        store
            .ensure_thumbnail(&single(ID, DIGEST), Some("<p>x</p>"))
            .await
            .is_some()
    );

    assert_eq!(entries(&dir), vec![format!("{DIGEST}.png")]);
}

#[tokio::test]
async fn removing_an_artifact_removes_its_preview_directory() {
    let data = TempDataDir::new("u16-remove");
    let stub = StubRenderer::rendering(sample_png());
    let store = thumbnail_store(data.path(), stub.config());
    assert!(
        store
            .ensure_thumbnail(&single(ID, DIGEST), Some("<p>x</p>"))
            .await
            .is_some()
    );
    assert!(
        store
            .ensure_thumbnail(&single(OTHER_ID, DIGEST), Some("<p>y</p>"))
            .await
            .is_some()
    );

    store.remove_artifact(&ArtifactId(ID.to_owned())).await;
    assert_eq!(
        entries(&data.path().join("previews")),
        vec![OTHER_ID.to_owned()]
    );

    // An unusable id is a silent no-op, and removing twice is not an error.
    store
        .remove_artifact(&ArtifactId("../previews".to_owned()))
        .await;
    store.remove_artifact(&ArtifactId(ID.to_owned())).await;
    assert_eq!(
        entries(&data.path().join("previews")),
        vec![OTHER_ID.to_owned()]
    );
}

// ---------------------------------------------------------------------------
// Startup audit
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_audit_removes_orphans_stale_files_and_invalid_pngs() {
    let data = TempDataDir::new("u16-audit");
    let previews = data.path().join("previews");
    let live = previews.join(ID);
    let bundle_dir = previews.join(OTHER_ID);
    let orphan = previews.join("orphan999999");
    std::fs::create_dir_all(&live).expect("live dir");
    std::fs::create_dir_all(&bundle_dir).expect("bundle dir");
    std::fs::create_dir_all(&orphan).expect("orphan dir");
    std::fs::write(previews.join("NOT-AN-ID"), b"loose file").expect("loose file");

    std::fs::write(live.join(format!("{DIGEST}.png")), sample_png()).expect("current");
    std::fs::write(live.join(format!("{OTHER_DIGEST}.png")), sample_png()).expect("stale");
    std::fs::write(live.join(".partial.tmp"), b"interrupted").expect("partial");
    std::fs::write(bundle_dir.join(format!("{DIGEST}.png")), sample_png()).expect("bundle png");

    let mut index = std::collections::HashMap::new();
    index.insert(
        ID.to_owned(),
        PreviewArtifactRef {
            is_bundle: false,
            body_sha256: DIGEST.to_owned(),
        },
    );
    index.insert(
        OTHER_ID.to_owned(),
        PreviewArtifactRef {
            is_bundle: true,
            body_sha256: DIGEST.to_owned(),
        },
    );

    let store = thumbnail_store(data.path(), PreviewConfig::default());
    let report = store.audit(&index as &dyn PreviewArtifactIndex).await;

    assert_eq!(
        report.orphan_dirs,
        vec!["NOT-AN-ID".to_owned(), "orphan999999".to_owned()]
    );
    assert_eq!(
        report.partial_files,
        vec![
            format!("{ID}/.partial.tmp"),
            format!("{ID}/{OTHER_DIGEST}.png"),
            // A bundle has no current digest, so every file under it is stale.
            format!("{OTHER_ID}/{DIGEST}.png"),
        ]
    );
    assert!(report.invalid_files.is_empty());
    assert!(!report.is_empty());

    assert_eq!(entries(&previews), vec![ID.to_owned(), OTHER_ID.to_owned()]);
    assert_eq!(entries(&live), vec![format!("{DIGEST}.png")]);
    assert!(entries(&bundle_dir).is_empty());
}

#[tokio::test]
async fn the_audit_deletes_a_current_file_that_is_not_a_png() {
    let data = TempDataDir::new("u16-audit-invalid");
    let live = data.path().join("previews").join(ID);
    std::fs::create_dir_all(&live).expect("live dir");
    std::fs::write(live.join(format!("{DIGEST}.png")), b"not a png").expect("invalid");

    let mut index = std::collections::HashMap::new();
    index.insert(
        ID.to_owned(),
        PreviewArtifactRef {
            is_bundle: false,
            body_sha256: DIGEST.to_owned(),
        },
    );

    let store = thumbnail_store(data.path(), PreviewConfig::default());
    let report = store.audit(&index as &dyn PreviewArtifactIndex).await;

    assert_eq!(report.invalid_files, vec![format!("{ID}/{DIGEST}.png")]);
    assert!(entries(&live).is_empty());
}

#[tokio::test]
async fn the_audit_creates_the_previews_directory_and_reports_nothing() {
    let data = TempDataDir::new("u16-audit-empty");
    let index = std::collections::HashMap::new();
    let store = thumbnail_store(data.path(), PreviewConfig::default());
    let report = store.audit(&index as &dyn PreviewArtifactIndex).await;
    assert!(report.is_empty());
    assert!(data.path().join("previews").is_dir());
}

// ---------------------------------------------------------------------------
// Coalescing and the serial priority lane
// ---------------------------------------------------------------------------

#[tokio::test]
async fn concurrent_ensure_calls_share_one_render_and_one_write() {
    let data = TempDataDir::new("u16-coalesce");
    let png = sample_png();
    let stub = StubRenderer::rendering(png.clone());
    stub.gate();
    let store = thumbnail_store(data.path(), stub.config());

    let mut waiters = Vec::new();
    for _ in 0..6 {
        let store = Arc::clone(&store);
        waiters.push(tokio::spawn(async move {
            store
                .ensure_thumbnail(&single(ID, DIGEST), Some("<p>x</p>"))
                .await
        }));
    }

    stub.wait_for_requests(1).await;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert_eq!(
        stub.request_count(),
        1,
        "concurrent ensures were not coalesced"
    );

    stub.release(1);
    for waiter in waiters {
        assert_eq!(waiter.await.expect("ensure task"), Some(png.clone()));
    }
    assert_eq!(stub.request_count(), 1);
    assert_eq!(
        entries(&data.path().join("previews").join(ID)),
        vec![format!("{DIGEST}.png")]
    );
}

#[tokio::test]
async fn the_queue_is_serial_and_runs_high_priority_work_first() {
    let data = TempDataDir::new("u16-queue-order");
    let stub = StubRenderer::rendering(sample_png());
    stub.gate();
    let queue = ThumbnailQueue::new(thumbnail_store(data.path(), stub.config()));

    // `first` occupies the single lane and blocks on the gated sidecar.
    let first = queue.enqueue(
        single("aaa111aaa111", DIGEST),
        PreviewHtml::Ready("first".to_owned()),
        PreviewPriority::High,
    );
    stub.wait_for_requests(1).await;

    // Enqueued while the lane is busy: the later high-priority job must overtake the low one.
    let low = queue.enqueue(
        single("bbb222bbb222", DIGEST),
        PreviewHtml::Ready("low".to_owned()),
        PreviewPriority::Low,
    );
    let high = queue.enqueue(
        single("ccc333ccc333", DIGEST),
        PreviewHtml::Ready("high".to_owned()),
        PreviewPriority::High,
    );
    assert_eq!(stub.request_count(), 1, "the lane is not serial");
    let depth = queue.depth();
    assert_eq!((depth.high, depth.low, depth.running), (1, 1, true));

    stub.release(3);
    assert!(first.await.is_some());
    assert!(high.await.is_some());
    assert!(low.await.is_some());

    assert_eq!(stub.received_html(), vec!["first", "high", "low"]);
    assert_eq!(queue.depth().high, 0);
}

#[tokio::test]
async fn the_queue_rejects_renderer_floods_without_growing_unboundedly() {
    let data = TempDataDir::new("u16-queue-capacity");
    let stub = StubRenderer::rendering(sample_png());
    stub.gate();
    let queue = ThumbnailQueue::new_with_limit(thumbnail_store(data.path(), stub.config()), 1);
    let first = queue.enqueue(
        single("aaa111aaa111", DIGEST),
        PreviewHtml::Ready("first".to_owned()),
        PreviewPriority::High,
    );
    stub.wait_for_requests(1).await;
    let queued = queue.enqueue(
        single("bbb222bbb222", DIGEST),
        PreviewHtml::Ready("queued".to_owned()),
        PreviewPriority::Low,
    );
    let rejected = queue.enqueue(
        single("ccc333ccc333", DIGEST),
        PreviewHtml::Ready("rejected".to_owned()),
        PreviewPriority::Low,
    );
    assert_eq!(queue.depth().low, 1);
    assert_eq!(rejected.await, None);
    stub.release(2);
    assert!(first.await.is_some());
    assert!(queued.await.is_some());
    assert_eq!(stub.received_html(), vec!["first", "queued"]);
}

#[tokio::test]
async fn queue_pressure_reports_idle_after_the_last_render_finishes() {
    let data = TempDataDir::new("u16-queue-pressure-idle");
    let stub = StubRenderer::rendering(sample_png());
    let ingress_config = AppConfig::default();
    let ingress = IngressState::from_config(&ingress_config);
    let queue = ThumbnailQueue::new(thumbnail_store(data.path(), stub.config()))
        .with_pressure(ingress.preview_queue_pressure());

    assert!(
        queue
            .enqueue(
                single("aaa111aaa111", DIGEST),
                PreviewHtml::Ready("finished".to_owned()),
                PreviewPriority::High,
            )
            .await
            .is_some()
    );
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        loop {
            if !queue.depth().running {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("queue reaches idle");
    assert!(
        ingress
            .render_prometheus()
            .contains("artifact_mcp_render_queue_running 0\n"),
        "idle transition must reach the exported ingress gauge"
    );
}

#[tokio::test]
async fn the_queue_reserves_declared_bytes_until_the_active_render_finishes() {
    let data = TempDataDir::new("u16-queue-byte-capacity");
    let stub = StubRenderer::rendering(sample_png());
    stub.gate();
    let queue = ThumbnailQueue::new_with_limits_and_counter(
        thumbnail_store(data.path(), stub.config()),
        4,
        64,
        None,
    );
    let first = queue.enqueue(
        single("aaa111aaa111", DIGEST),
        PreviewHtml::Ready("first".to_owned()),
        PreviewPriority::High,
    );
    stub.wait_for_requests(1).await;
    let rejected = queue.enqueue(
        single("bbb222bbb222", DIGEST),
        PreviewHtml::Ready("second".to_owned()),
        PreviewPriority::High,
    );
    assert_eq!(queue.depth().reserved_bytes, 42);
    assert_eq!(rejected.await, None);
    stub.release(1);
    assert!(first.await.is_some());
    assert_eq!(queue.depth().reserved_bytes, 0);
}

#[tokio::test]
async fn a_deferred_job_can_decline_to_render() {
    // The startup backfill re-reads the body when the job runs and returns `None` if a
    // concurrent update moved the digest on — a stale body must never overwrite the thumbnail.
    let data = TempDataDir::new("u16-queue-deferred");
    let stub = StubRenderer::rendering(sample_png());
    let queue = ThumbnailQueue::new(thumbnail_store(data.path(), stub.config()));

    let declined = queue.enqueue(
        single(ID, DIGEST),
        PreviewHtml::Deferred(Box::new(|| Box::pin(async { None }))),
        PreviewPriority::Low,
    );
    assert_eq!(declined.await, None);
    assert_eq!(stub.request_count(), 0);

    let rendered = queue.enqueue(
        single(ID, DIGEST),
        PreviewHtml::Deferred(Box::new(|| {
            Box::pin(async { Some("<p>late</p>".to_owned()) })
        })),
        PreviewPriority::Low,
    );
    assert!(rendered.await.is_some());
    assert_eq!(stub.received_html(), vec!["<p>late</p>"]);
}

// ---------------------------------------------------------------------------
// Optional by design: the port never fails a caller
// ---------------------------------------------------------------------------

/// Every failure mode a publish/update/restore could hit, driven through the frozen port.
#[tokio::test]
async fn the_preview_service_never_fails_the_caller() {
    let good = sample_png();
    let cases: Vec<(&str, StubReply)> = vec![
        ("renderer busy", StubReply::renderer_busy()),
        ("render failed", StubReply::render_failed()),
        ("empty body", StubReply::Png(Vec::new())),
        ("not a png", StubReply::Png(b"<html>nope</html>".to_vec())),
        (
            "wrong content type",
            StubReply::Chunked {
                content_type: "text/html".to_owned(),
                body: good.clone(),
                chunk: 32,
            },
        ),
        (
            "lying content length",
            StubReply::Declared {
                declared: u64::from(u32::MAX),
                body: good.clone(),
            },
        ),
        (
            "redirect",
            StubReply::Redirect("http://example.invalid/".to_owned()),
        ),
        ("silence", StubReply::Hang),
    ];

    for (label, reply) in cases {
        let data = TempDataDir::new("u16-optional");
        let stub = StubRenderer::start(reply);
        let config = PreviewConfig {
            timeout_ms: 250,
            ..stub.config()
        };
        let service = PreviewIntegration::new(thumbnail_store(data.path(), config));
        let meta = single(ID, DIGEST);

        assert!(
            service.enabled(),
            "{label}: a configured renderer reports enabled"
        );
        assert_eq!(
            service
                .ensure_thumbnail(&meta, "<p>x</p>", PreviewPriority::High)
                .await,
            Ok(None),
            "{label}: the port must degrade, never fail"
        );
        assert_eq!(service.remove_artifact(&meta.id).await, Ok(()), "{label}");
        assert!(!service.placeholder(&meta, None).is_empty(), "{label}");
    }
}

#[tokio::test]
async fn an_unreachable_sidecar_never_fails_the_caller() {
    let data = TempDataDir::new("u16-unreachable");
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("probe socket");
    let address = listener.local_addr().expect("probe address");
    drop(listener);

    let service = PreviewIntegration::new(thumbnail_store(
        data.path(),
        PreviewConfig {
            renderer_endpoint: Some(format!("http://{address}/render")),
            timeout_ms: 250,
            ..PreviewConfig::default()
        },
    ));
    let meta = single(ID, DIGEST);
    assert_eq!(
        service
            .ensure_thumbnail(&meta, "<p>x</p>", PreviewPriority::High)
            .await,
        Ok(None)
    );
    assert_eq!(service.remove_artifact(&meta.id).await, Ok(()));
    assert!(entries(&data.path().join("previews")).is_empty());
}

#[tokio::test]
async fn a_disabled_sidecar_is_an_inert_service() {
    let data = TempDataDir::new("u16-disabled");
    let service = PreviewIntegration::from_config(&AppConfig {
        data_dir: data.path().to_path_buf(),
        ..AppConfig::defaults()
    });
    let meta = single(ID, DIGEST);

    assert!(!service.enabled());
    assert_eq!(
        service
            .ensure_thumbnail(&meta, "<p>x</p>", PreviewPriority::High)
            .await,
        Ok(None)
    );
    assert_eq!(service.remove_artifact(&meta.id).await, Ok(()));
    // Nothing was created: a disabled renderer does not even touch the data directory, and the
    // serial lane — which exists only to serialise renders — is never started.
    assert!(!data.path().join("previews").exists());
    assert!(!service.queue().depth().running);
    assert!(!service.store().enabled());
}

#[tokio::test]
async fn a_disabled_sidecar_still_serves_a_thumbnail_left_on_disk() {
    // Turning the `preview` compose profile off must not blind the service to thumbnails an
    // earlier run already rendered: Node checks the existing file before `renderer?.enabled`.
    let data = TempDataDir::new("u16-disabled-existing");
    let png = sample_png();
    let target = expected_path(data.path(), ID, DIGEST);
    std::fs::create_dir_all(target.parent().expect("parent")).expect("preview dir");
    std::fs::write(&target, &png).expect("existing thumbnail");

    let service = PreviewIntegration::new(thumbnail_store(data.path(), PreviewConfig::default()));
    assert!(!service.enabled());
    assert_eq!(
        service
            .ensure_thumbnail(&single(ID, DIGEST), "<p>x</p>", PreviewPriority::High)
            .await,
        Ok(Some(png))
    );
}

#[tokio::test]
async fn the_placeholder_is_always_available() {
    let data = TempDataDir::new("u16-placeholder");
    let service = PreviewIntegration::new(thumbnail_store(data.path(), PreviewConfig::default()));

    let svg = String::from_utf8(service.placeholder(&single(ID, DIGEST), Some("#123456")))
        .expect("utf-8 svg");
    assert!(svg.contains("fill=\"#123456\""));
    assert!(svg.contains("Preview temporarily unavailable"));

    let bundle =
        String::from_utf8(service.placeholder(&meta(ID, DIGEST, true), None)).expect("utf-8 svg");
    assert!(bundle.contains("Bundle preview"));
    assert!(
        bundle.contains("hsl("),
        "an unusable accent falls back to the org hue"
    );
}

// ---------------------------------------------------------------------------
// Validation surface
// ---------------------------------------------------------------------------

#[test]
fn png_validation_bounds() {
    assert!(valid_png(&sample_png(), DEFAULT_MAX_PNG_BYTES));
    assert!(!valid_png(&[], DEFAULT_MAX_PNG_BYTES));
    assert!(!valid_png(b"\x89PNG", DEFAULT_MAX_PNG_BYTES));
    assert!(!valid_png(&png_of(64), 63));
    assert!(valid_png(&png_of(64), 64));
}
