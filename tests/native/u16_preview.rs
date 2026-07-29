//! U16: the preview-renderer HTTP client against a stubbed sidecar.
//!
//! Every test here drives [`PreviewRenderer`] over a real loopback socket served by
//! [`crate::u16_support::StubRenderer`], which speaks the sidecar contract from
//! `preview-renderer/server.js`. Nothing here needs Chromium, the `preview` compose profile, or
//! the real sidecar — that is deliberate: the renderer is optional in production and the suite
//! must prove the client's behaviour when it is absent, slow, or lying.
//!
//! The contract exercised:
//!
//! * `POST /render`, `content-type: application/json`, body `{"html":…,"width":…,"height":…}`;
//! * success is `200` + `content-type: image/png` + a PNG within the byte cap;
//! * everything else — `503 renderer busy`, `500 render failed`, a redirect, a mislabelled body,
//!   a lying `content-length`, an unbounded stream, silence — yields `None`, never an error.

use std::sync::Arc;
use std::time::{Duration, Instant};

use artifact_mcp::config::PreviewConfig;
use artifact_mcp::integrations::preview::PreviewRenderer;
use artifact_mcp::integrations::thumbnails::PNG_SIGNATURE;

use crate::u16_support::{StubRenderer, StubReply, png_of, sample_png};

const ID: &str = "abc123def456";
const DIGEST: &str = "cafebabe00112233445566778899aabbccddeeff00112233445566778899aabb";

fn renderer(config: PreviewConfig) -> Arc<PreviewRenderer> {
    Arc::new(PreviewRenderer::new(&config))
}

/// A port nothing is listening on: bind, read the address, drop the listener.
fn dead_endpoint() -> String {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind probe socket");
    let address = listener.local_addr().expect("probe address");
    drop(listener);
    format!("http://{address}/render")
}

// ---------------------------------------------------------------------------
// The happy path and the request the sidecar actually receives
// ---------------------------------------------------------------------------

#[tokio::test]
async fn posts_the_sidecar_contract_and_returns_the_png() {
    let png = sample_png();
    let stub = StubRenderer::rendering(png.clone());
    let renderer = renderer(PreviewConfig {
        viewport_width: 800,
        viewport_height: 400,
        ..stub.config()
    });

    assert_eq!(renderer.render_preview("<h1>hello</h1>").await, Some(png));

    let requests = stub.requests();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(request.method, "POST");
    assert_eq!(request.target, "/render");
    assert!(
        request.content_type.starts_with("application/json"),
        "content-type was {:?}",
        request.content_type
    );
    assert_eq!(request.html, "<h1>hello</h1>");
    assert_eq!((request.width, request.height), (800, 400));
}

#[tokio::test]
async fn the_default_viewport_is_the_frozen_1200x630() {
    let stub = StubRenderer::rendering(sample_png());
    let renderer = renderer(stub.config());
    assert!(renderer.render_preview("<p>x</p>").await.is_some());
    let request = stub.requests().remove(0);
    assert_eq!((request.width, request.height), (1200, 630));
}

// ---------------------------------------------------------------------------
// Invalid PNG rejection
// ---------------------------------------------------------------------------

#[tokio::test]
async fn rejects_a_bad_png_signature() {
    let mut broken = sample_png();
    broken[7] = 0x0b; // last signature byte flipped
    let stub = StubRenderer::rendering(broken);
    assert_eq!(
        renderer(stub.config()).render_preview("<p>x</p>").await,
        None
    );
    assert_eq!(stub.request_count(), 1, "the request was still made");
}

#[tokio::test]
async fn rejects_an_empty_body() {
    let stub = StubRenderer::rendering(Vec::new());
    assert_eq!(
        renderer(stub.config()).render_preview("<p>x</p>").await,
        None
    );
}

#[tokio::test]
async fn rejects_a_body_shorter_than_the_signature() {
    let stub = StubRenderer::rendering(PNG_SIGNATURE[..7].to_vec());
    assert_eq!(
        renderer(stub.config()).render_preview("<p>x</p>").await,
        None
    );
}

#[tokio::test]
async fn rejects_a_json_error_page_mislabelled_as_png() {
    let stub = StubRenderer::start(StubReply::Declared {
        declared: 25,
        body: br#"{"error":"render failed"}"#.to_vec(),
    });
    assert_eq!(
        renderer(stub.config()).render_preview("<p>x</p>").await,
        None
    );
}

#[tokio::test]
async fn rejects_a_declared_length_over_the_cap() {
    // The cap is enforced from `content-length` before a single body byte is buffered.
    let stub = StubRenderer::start(StubReply::Declared {
        declared: 5_000,
        body: png_of(64),
    });
    let renderer = renderer(PreviewConfig {
        max_png_bytes: 1_024,
        ..stub.config()
    });
    assert_eq!(renderer.render_preview("<p>x</p>").await, None);
}

#[tokio::test]
async fn rejects_a_streamed_body_over_the_cap() {
    // No `content-length` at all: the cap has to be enforced while reading, and the read must
    // stop rather than buffer an unbounded body into the process.
    let stub = StubRenderer::start(StubReply::Chunked {
        content_type: "image/png".to_owned(),
        body: png_of(8_192),
        chunk: 256,
    });
    let renderer = renderer(PreviewConfig {
        max_png_bytes: 1_024,
        ..stub.config()
    });
    assert_eq!(renderer.render_preview("<p>x</p>").await, None);
}

#[tokio::test]
async fn accepts_a_png_exactly_at_the_cap_and_rejects_one_byte_more() {
    let at_cap = png_of(1_024);
    let stub = StubRenderer::rendering(at_cap.clone());
    let renderer = renderer(PreviewConfig {
        max_png_bytes: 1_024,
        ..stub.config()
    });
    assert_eq!(renderer.render_preview("<p>x</p>").await, Some(at_cap));

    let over = StubRenderer::rendering(png_of(1_025));
    let renderer = renderer_for(&over, 1_024);
    assert_eq!(renderer.render_preview("<p>x</p>").await, None);
}

fn renderer_for(stub: &StubRenderer, max_png_bytes: u64) -> Arc<PreviewRenderer> {
    renderer(PreviewConfig {
        max_png_bytes,
        ..stub.config()
    })
}

// ---------------------------------------------------------------------------
// Sidecar error shapes
// ---------------------------------------------------------------------------

#[tokio::test]
async fn rejects_every_non_success_status() {
    for reply in [StubReply::renderer_busy(), StubReply::render_failed()] {
        let stub = StubRenderer::start(reply);
        assert_eq!(
            renderer(stub.config()).render_preview("<p>x</p>").await,
            None
        );
    }
}

#[tokio::test]
async fn rejects_a_non_png_content_type() {
    let stub = StubRenderer::start(StubReply::Chunked {
        content_type: "image/webp".to_owned(),
        body: sample_png(),
        chunk: 64,
    });
    assert_eq!(
        renderer(stub.config()).render_preview("<p>x</p>").await,
        None
    );
}

#[tokio::test]
async fn never_follows_a_redirect() {
    // `redirect: "error"` — a renderer that redirects is a renderer that is not answering.
    let good = StubRenderer::rendering(sample_png());
    let stub = StubRenderer::start(StubReply::Redirect(good.endpoint()));
    assert_eq!(
        renderer(stub.config()).render_preview("<p>x</p>").await,
        None
    );
    assert_eq!(good.request_count(), 0, "the redirect target was contacted");
}

// ---------------------------------------------------------------------------
// Timeout and unavailability
// ---------------------------------------------------------------------------

#[tokio::test]
async fn times_out_a_silent_renderer() {
    let stub = StubRenderer::start(StubReply::Hang);
    let renderer = renderer(PreviewConfig {
        timeout_ms: 250,
        ..stub.config()
    });

    let started = Instant::now();
    assert_eq!(renderer.render_preview("<p>x</p>").await, None);
    let elapsed = started.elapsed();
    assert!(
        elapsed >= Duration::from_millis(200),
        "returned too early: {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_secs(5),
        "the timeout did not fire: {elapsed:?}"
    );
    assert_eq!(stub.request_count(), 1);
}

#[tokio::test]
async fn a_timeout_is_not_remembered() {
    // A hung render must be evicted so the next caller retries rather than inheriting the
    // failure for the lifetime of the process.
    let stub = StubRenderer::rendering(sample_png());
    stub.push_reply(StubReply::Hang);
    let renderer = renderer(PreviewConfig {
        timeout_ms: 250,
        ..stub.config()
    });

    assert_eq!(
        renderer
            .render_revision_preview(ID, DIGEST, "<p>x</p>")
            .await,
        None
    );
    assert!(renderer.cache_keys().is_empty(), "a failure stayed cached");
    assert!(
        renderer
            .render_revision_preview(ID, DIGEST, "<p>x</p>")
            .await
            .is_some()
    );
    assert_eq!(stub.request_count(), 2);
}

#[tokio::test]
async fn an_unreachable_renderer_is_a_clean_no_op() {
    // The sidecar's compose profile is off: connections are refused, and the client must simply
    // report "no preview" without an error, a panic, or a stall.
    let renderer = renderer(PreviewConfig {
        renderer_endpoint: Some(dead_endpoint()),
        ..PreviewConfig::default()
    });
    assert!(
        renderer.enabled(),
        "configuration alone still counts as enabled"
    );
    assert_eq!(renderer.render_preview("<p>x</p>").await, None);
    assert_eq!(
        renderer
            .render_revision_preview(ID, DIGEST, "<p>x</p>")
            .await,
        None
    );
    assert!(renderer.cache_keys().is_empty());
}

#[tokio::test]
async fn a_disabled_renderer_never_opens_a_socket() {
    let stub = StubRenderer::rendering(sample_png());
    let renderer = renderer(PreviewConfig {
        renderer_endpoint: None,
        ..stub.config()
    });
    assert!(!renderer.enabled());
    assert_eq!(renderer.render_preview("<p>x</p>").await, None);
    assert_eq!(
        renderer
            .render_revision_preview(ID, DIGEST, "<p>x</p>")
            .await,
        None
    );
    assert_eq!(stub.request_count(), 0);
}

// ---------------------------------------------------------------------------
// Cache and coalescing
// ---------------------------------------------------------------------------

#[tokio::test]
async fn coalesces_concurrent_identical_requests_into_one_render() {
    let png = sample_png();
    let stub = StubRenderer::rendering(png.clone());
    stub.gate();
    let renderer = renderer(stub.config());

    let mut waiters = Vec::new();
    for _ in 0..8 {
        let renderer = Arc::clone(&renderer);
        waiters.push(tokio::spawn(async move {
            renderer
                .render_revision_preview(ID, DIGEST, "<p>x</p>")
                .await
        }));
    }

    // Exactly one render reaches the sidecar; the other seven are still parked on it.
    stub.wait_for_requests(1).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        stub.request_count(),
        1,
        "identical requests were not coalesced"
    );

    stub.release(1);
    for waiter in waiters {
        assert_eq!(waiter.await.expect("render task"), Some(png.clone()));
    }
    assert_eq!(stub.request_count(), 1);
}

#[tokio::test]
async fn a_successful_render_is_cached_and_distinct_keys_are_not() {
    let stub = StubRenderer::rendering(sample_png());
    let renderer = renderer(stub.config());

    let first = renderer
        .render_revision_preview(ID, DIGEST, "<p>x</p>")
        .await;
    let second = renderer
        .render_revision_preview(ID, DIGEST, "<p>x</p>")
        .await;
    assert_eq!(first, second);
    assert!(first.is_some());
    assert_eq!(stub.request_count(), 1, "the cached render was repeated");

    // A different revision of the same artifact is a different key.
    assert!(
        renderer
            .render_revision_preview(ID, &DIGEST.replace('a', "b"), "<p>y</p>")
            .await
            .is_some()
    );
    assert_eq!(stub.request_count(), 2);
    assert_eq!(renderer.cache_keys().len(), 2);
}

#[tokio::test]
async fn a_failed_render_is_not_cached() {
    let stub = StubRenderer::rendering(sample_png());
    stub.push_reply(StubReply::render_failed());
    let renderer = renderer(stub.config());

    assert_eq!(
        renderer
            .render_revision_preview(ID, DIGEST, "<p>x</p>")
            .await,
        None
    );
    assert!(
        renderer
            .render_revision_preview(ID, DIGEST, "<p>x</p>")
            .await
            .is_some()
    );
    assert_eq!(stub.request_count(), 2);
}

#[tokio::test]
async fn the_cache_is_bounded_and_evicts_the_least_recently_used_key() {
    let stub = StubRenderer::rendering(sample_png());
    let renderer = renderer(PreviewConfig {
        cache_entries: 2,
        ..stub.config()
    });

    for digest in ["a", "b"] {
        assert!(
            renderer
                .render_revision_preview(ID, digest, "<p>x</p>")
                .await
                .is_some()
        );
    }
    // Refresh `a`, then insert `c`: `b` is the least recently used and is evicted.
    assert!(
        renderer
            .render_revision_preview(ID, "a", "<p>x</p>")
            .await
            .is_some()
    );
    assert!(
        renderer
            .render_revision_preview(ID, "c", "<p>x</p>")
            .await
            .is_some()
    );
    assert_eq!(
        renderer.cache_keys(),
        vec![format!("{ID}:a"), format!("{ID}:c")]
    );
    assert_eq!(stub.request_count(), 3);

    // The evicted key has to be rendered again.
    assert!(
        renderer
            .render_revision_preview(ID, "b", "<p>x</p>")
            .await
            .is_some()
    );
    assert_eq!(stub.request_count(), 4);
}

#[tokio::test]
async fn an_uncached_render_never_consults_the_cache() {
    // `render_preview` is the raw call the notifier path uses; it must not be memoised.
    let stub = StubRenderer::rendering(sample_png());
    let renderer = renderer(stub.config());
    for _ in 0..3 {
        assert!(renderer.render_preview("<p>x</p>").await.is_some());
    }
    assert_eq!(stub.request_count(), 3);
    assert!(renderer.cache_keys().is_empty());
}
