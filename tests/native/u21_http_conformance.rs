use std::sync::Arc;

use artifact_mcp::{
    config::AccessIdentityMode,
    http::middleware::{
        AccessRetryState, access_session_retry, express_etag, prevent_response_transforms,
        weak_etag,
    },
    ports::PageRenderer,
    render::portal::AskamaPageRenderer,
};
use axum::{
    Json, Router,
    body::{Body, Bytes, to_bytes},
    http::{Method, Request, StatusCode, header},
    middleware,
    response::{IntoResponse, Response},
    routing::get,
};
use futures_util::StreamExt;
use serde_json::json;
use std::time::Duration;
use tower::ServiceExt;

#[test]
fn weak_etag_matches_the_frozen_express_etag_vectors() {
    let samples: [&[u8]; 4] = [
        b"",
        b"<!doctype html><p>hi</p>",
        "ETags hash UTF-8 bytes: \u{1f389}".as_bytes(),
        &[0, 1, 2, 127, 128, 254, 255],
    ];
    // Frozen vector: captured from the retired Node runtime at commit 7211de0.
    let expected = [
        "W/\"0-2jmj7l5rSw0yVb/vlWAYkK/YBwk\"",
        "W/\"18-UaVUwJRaEdYpGlgIIEN1QooEbkw\"",
        "W/\"1c-hYu6oZsp6IAviEb4dV0zVUkblYA\"",
        "W/\"7-tKcZUKYW/xfCNelzHFjTj8cD/yM\"",
    ];
    let actual = samples
        .iter()
        .map(|sample| weak_etag(sample))
        .collect::<Vec<_>>();
    assert_eq!(
        actual, expected,
        "the weak ETag is `W/\"<hex length>-<base64 SHA-1, 27 chars>\"`, with a fixed empty-body value"
    );
}

async fn send_body() -> Response {
    (
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        "<!doctype html><p>hi</p>",
    )
        .into_response()
}

async fn json_error() -> Response {
    (StatusCode::NOT_FOUND, Json(json!({ "error": "Not found" }))).into_response()
}

async fn redirect() -> Response {
    let mut response = Response::new(Body::from("Found. Redirecting to /target"));
    *response.status_mut() = StatusCode::FOUND;
    response.headers_mut().insert(
        header::LOCATION,
        "/target".parse().expect("redirect location"),
    );
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        "text/plain; charset=utf-8".parse().expect("content type"),
    );
    response
}

async fn bodyless() -> StatusCode {
    StatusCode::ACCEPTED
}

fn etag_app() -> Router {
    async fn no_content_type() -> Response {
        Response::new(Body::from("fallback body"))
    }

    Router::new()
        .route("/send", get(send_body))
        .route("/json-error", get(json_error))
        .route("/redirect", get(redirect))
        .route("/bodyless", get(bodyless))
        .route("/bare", get(no_content_type))
        .layer(middleware::from_fn(express_etag))
}

async fn request(method: Method, path: &str, headers: &[(&str, &str)]) -> Response {
    let mut builder = Request::builder().method(method).uri(path);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    etag_app()
        .oneshot(builder.body(Body::empty()).expect("request"))
        .await
        .expect("response")
}

#[tokio::test]
async fn etag_middleware_matches_express_send_json_redirect_and_end_paths() {
    let sent = request(Method::GET, "/send", &[]).await;
    assert_eq!(sent.status(), StatusCode::OK);
    assert_eq!(
        sent.headers()[header::ETAG],
        "W/\"18-UaVUwJRaEdYpGlgIIEN1QooEbkw\""
    );

    let json = request(Method::GET, "/json-error", &[]).await;
    assert_eq!(json.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        json.headers()[header::ETAG],
        weak_etag(br#"{"error":"Not found"}"#)
    );

    for path in ["/redirect", "/bodyless", "/bare"] {
        let response = request(Method::GET, path, &[]).await;
        assert!(
            !response.headers().contains_key(header::ETAG),
            "Express's redirect/end/finalhandler-equivalent path {path} is not ETagged"
        );
    }
}

#[tokio::test]
async fn etag_middleware_preserves_head_and_express_conditional_get_semantics() {
    let head = request(Method::HEAD, "/send", &[]).await;
    assert_eq!(
        head.headers()[header::ETAG],
        "W/\"18-UaVUwJRaEdYpGlgIIEN1QooEbkw\""
    );
    assert_eq!(
        to_bytes(head.into_body(), 1)
            .await
            .expect("HEAD body")
            .len(),
        0
    );

    let fresh = request(
        Method::GET,
        "/send",
        &[("if-none-match", "W/\"18-UaVUwJRaEdYpGlgIIEN1QooEbkw\"")],
    )
    .await;
    assert_eq!(fresh.status(), StatusCode::NOT_MODIFIED);
    assert_eq!(
        fresh.headers()[header::ETAG],
        "W/\"18-UaVUwJRaEdYpGlgIIEN1QooEbkw\""
    );
    assert!(!fresh.headers().contains_key(header::CONTENT_TYPE));
    assert!(!fresh.headers().contains_key(header::CONTENT_LENGTH));
    assert!(
        to_bytes(fresh.into_body(), 1)
            .await
            .expect("304 body")
            .is_empty()
    );

    let reload = request(
        Method::GET,
        "/send",
        &[
            ("if-none-match", "W/\"18-UaVUwJRaEdYpGlgIIEN1QooEbkw\""),
            ("cache-control", "no-cache"),
        ],
    )
    .await;
    assert_eq!(reload.status(), StatusCode::OK);
    let body = to_bytes(reload.into_body(), 1024)
        .await
        .expect("reload body");
    assert_eq!(body, "<!doctype html><p>hi</p>");
}

#[tokio::test]
async fn etag_middleware_passes_streamed_bodies_through_without_buffering() {
    // A streamed body (NDJSON data, PCM speech) has no exact size. Express never ETags a stream,
    // and buffering one would hold every byte until the upstream finishes.
    async fn first_chunk_then_wait(content_type: &'static str) -> Response {
        let chunks = futures_util::stream::once(async {
            Ok::<_, std::convert::Infallible>(Bytes::from_static(b"{\"n\":1}\n"))
        })
        .chain(futures_util::stream::pending());
        (
            [(header::CONTENT_TYPE, content_type)],
            Body::from_stream(chunks),
        )
            .into_response()
    }

    for content_type in [
        "application/x-ndjson; charset=utf-8",
        "application/vnd.artifact.pcm",
        "text/event-stream",
    ] {
        let app = Router::new()
            .route("/stream", get(move || first_chunk_then_wait(content_type)))
            .layer(middleware::from_fn(express_etag));
        let response = tokio::time::timeout(
            Duration::from_secs(2),
            app.oneshot(
                Request::get("/stream")
                    .body(Body::empty())
                    .expect("request"),
            ),
        )
        .await
        .unwrap_or_else(|_| panic!("{content_type}: headers were held until the stream ended"))
        .expect("response");
        assert!(
            !response.headers().contains_key(header::ETAG),
            "{content_type}: a stream is not ETagged"
        );

        let mut body = response.into_body().into_data_stream();
        let first = tokio::time::timeout(Duration::from_secs(2), body.next())
            .await
            .unwrap_or_else(|_| panic!("{content_type}: the first chunk was buffered"))
            .expect("first chunk")
            .expect("chunk bytes");
        assert_eq!(first, "{\"n\":1}\n");
    }
}

#[tokio::test]
async fn access_session_retry_stays_outside_express_and_bypasses_etag() {
    let pages: Arc<dyn PageRenderer> = Arc::new(AskamaPageRenderer::default());
    let expected = pages
        .access_retry("/?cf_access_retry=1")
        .expect("render expected retry page");
    let app = Router::new()
        .route("/", get(send_body))
        .layer(middleware::from_fn(express_etag))
        .layer(middleware::from_fn_with_state(
            AccessRetryState::new(AccessIdentityMode::Jwt, pages),
            access_session_retry,
        ))
        .layer(middleware::from_fn(prevent_response_transforms));

    let response = app
        .oneshot(
            Request::builder()
                .uri("/")
                .header(header::COOKIE, "CF_Authorization=session-token")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()[header::CONTENT_TYPE],
        "text/html; charset=utf-8"
    );
    assert_eq!(
        response.headers()[header::CACHE_CONTROL],
        "no-store, no-transform"
    );
    assert_eq!(
        response.headers()[header::X_CONTENT_TYPE_OPTIONS],
        "nosniff"
    );
    assert_eq!(response.headers()[header::REFERRER_POLICY], "no-referrer");
    assert!(!response.headers().contains_key(header::ETAG));
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("retry body");
    assert_eq!(body, expected);
}
