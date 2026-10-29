use super::{body::*, middleware::*, state::*};
use axum::{
    Router,
    body::Body,
    http::{HeaderMap, HeaderValue, Method, Request, StatusCode, header},
    middleware,
    routing::get,
};
use std::{
    collections::VecDeque,
    convert::Infallible,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};
use tokio::sync::Notify;

use crate::{
    config::{AppConfig, IngressConfig},
    mcp::protocol::OrderedJson,
    model::PublisherIdentity,
};

struct ChunkedBody {
    chunks: VecDeque<axum::body::Bytes>,
}

impl http_body::Body for ChunkedBody {
    type Data = axum::body::Bytes;
    type Error = Infallible;

    fn poll_frame(
        mut self: std::pin::Pin<&mut Self>,
        _context: &mut Context<'_>,
    ) -> Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
        Poll::Ready(
            self.chunks
                .pop_front()
                .map(|chunk| Ok(http_body::Frame::data(chunk))),
        )
    }
}

struct StalledBody;

impl http_body::Body for StalledBody {
    type Data = axum::body::Bytes;
    type Error = Infallible;

    fn poll_frame(
        self: std::pin::Pin<&mut Self>,
        _context: &mut Context<'_>,
    ) -> Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
        let _ = self;
        Poll::Pending
    }
}

struct SlowDripBody {
    chunks: VecDeque<axum::body::Bytes>,
    delay: Duration,
    wait: Option<std::pin::Pin<Box<tokio::time::Sleep>>>,
}

impl http_body::Body for SlowDripBody {
    type Data = axum::body::Bytes;
    type Error = Infallible;

    fn poll_frame(
        mut self: std::pin::Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
        if self.chunks.is_empty() {
            return Poll::Ready(None);
        }
        if self.wait.is_none() {
            let delay = self.delay;
            self.wait = Some(Box::pin(tokio::time::sleep(delay)));
        }
        if self
            .wait
            .as_mut()
            .expect("delay initialized")
            .as_mut()
            .poll(context)
            .is_pending()
        {
            return Poll::Pending;
        }
        self.wait = None;
        Poll::Ready(
            self.chunks
                .pop_front()
                .map(|chunk| Ok(http_body::Frame::data(chunk))),
        )
    }
}
use tower::ServiceExt as _;

fn router(state: Arc<IngressState>) -> Router {
    Router::new()
        .route("/ok", get(|| async { "ok" }))
        .layer(middleware::from_fn_with_state(state, admit))
}

#[tokio::test]
async fn header_and_uri_limits_are_rejected_before_the_handler() {
    let mut config = AppConfig::default();
    config.ingress.max_headers = 1;
    config.ingress.max_uri_bytes = 2;
    let response = router(Arc::new(IngressState::from_config(&config)))
        .oneshot(
            Request::builder()
                .uri("/ok")
                .header("x-one", "1")
                .header("x-two", "2")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::URI_TOO_LONG);
}

#[tokio::test]
async fn admission_headers_precede_multipart_and_declared_body_limits() {
    let mut config = AppConfig::default();
    config.ingress.max_headers = 1;
    config.body.category_json = 1;
    let response = router(Arc::new(IngressState::from_config(&config)))
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/ok")
                .header(header::CONTENT_TYPE, "multipart/form-data")
                .header(header::CONTENT_LENGTH, "2")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE
    );
}

#[tokio::test]
async fn admission_multipart_precedes_declared_body_limit() {
    let mut config = AppConfig::default();
    config.body.category_json = 1;
    let response = router(Arc::new(IngressState::from_config(&config)))
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/ok")
                .header(header::CONTENT_TYPE, "multipart/form-data")
                .header(header::CONTENT_LENGTH, "2")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
}

#[tokio::test]
async fn admission_declared_body_limit_precedes_rate_limit() {
    let mut config = AppConfig::default();
    config.body.category_json = 1;
    config.body.feedback_json = 1;
    config.body.reaction_json = 1;
    config.ingress.mutations_per_window = 0;
    let response = router(Arc::new(IngressState::from_config(&config)))
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/ok")
                .header(header::CONTENT_LENGTH, "2")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn admission_rate_limit_precedes_request_concurrency_limit() {
    let mut config = AppConfig::default();
    config.ingress.mutations_per_window = 0;
    config.ingress.max_concurrent_requests = 0;
    let response = router(Arc::new(IngressState::from_config(&config)))
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/ok")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test]
async fn share_brute_force_and_bad_key_buckets_are_bounded_without_identity_labels() {
    let mut config = AppConfig::default();
    config.ingress.shares_per_window = 1;
    config.ingress.auth_failures_per_window = 1;
    let state = Arc::new(IngressState::from_config(&config));
    let app = Router::new()
        .route("/s/{token}", get(|| async { "missing" }))
        .layer(middleware::from_fn_with_state(Arc::clone(&state), admit));
    let first = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/s/not-a-share")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let second = app
        .oneshot(
            Request::builder()
                .uri("/s/not-a-share")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(first.status(), StatusCode::OK);
    assert_eq!(second.status(), StatusCode::TOO_MANY_REQUESTS);
    let mut headers = HeaderMap::new();
    headers.insert(
        header::AUTHORIZATION,
        HeaderValue::from_static("Bearer candidate"),
    );
    assert!(state.allow_auth_failure(&headers, None));
    assert!(!state.allow_auth_failure(&headers, None));
    assert!(!state.metrics().render_prometheus().contains("candidate"));
}

#[test]
fn rotating_bad_credentials_cannot_rotate_the_source_budget() {
    let mut config = AppConfig::default();
    config.ingress.auth_failures_per_window = 1;
    let state = IngressState::from_config(&config);
    let mut first = HeaderMap::new();
    first.insert(
        header::AUTHORIZATION,
        HeaderValue::from_static("Bearer first"),
    );
    let mut rotated = HeaderMap::new();
    rotated.insert(
        header::AUTHORIZATION,
        HeaderValue::from_static("Bearer second"),
    );
    let peer = Some("192.0.2.4:4444".parse().unwrap());
    assert!(state.allow_auth_failure(&first, peer));
    assert!(!state.allow_auth_failure(&rotated, peer));
}

#[test]
fn weighted_mcp_reservations_are_atomic_when_a_batch_exceeds_remaining_budget() {
    let mut config = AppConfig::default();
    config.ingress.reads_per_window = 3;
    let state = IngressState::from_config(&config);
    let publisher = PublisherIdentity {
        client_id: crate::model::ClientId::from("publisher"),
        org: crate::model::OrgId::from("acme"),
        label: "Publisher".to_owned(),
        role: "writer".to_owned(),
        scopes: None,
    };
    let headers = HeaderMap::new();

    assert!(state.allow_verified_publisher_operation_weight(
        &headers,
        None,
        &publisher,
        McpCost::Read,
        2,
    ));
    assert!(
        !state.allow_verified_publisher_operation_weight(
            &headers,
            None,
            &publisher,
            McpCost::Read,
            2,
        ),
        "the second batch is rejected as one reservation"
    );
    assert!(
        state.allow_verified_publisher_operation(&headers, None, &publisher, McpCost::Read),
        "a rejected weight-two batch must not consume the one token still available"
    );
}

#[test]
fn verified_upload_budget_is_scoped_to_the_verified_publisher_and_source() {
    let mut config = AppConfig::default();
    config.ingress.uploads_per_window = 1;
    let state = IngressState::from_config(&config);
    let publisher = PublisherIdentity {
        client_id: crate::model::ClientId("client-a".to_owned()),
        org: crate::model::OrgId("org-a".to_owned()),
        label: "test".to_owned(),
        role: "author".to_owned(),
        scopes: None,
    };
    let headers = HeaderMap::new();
    let peer = Some("192.0.2.4:4444".parse().unwrap());
    assert!(state.allow_verified_publisher_operation(&headers, peer, &publisher, McpCost::Upload,));
    assert!(
        !state.allow_verified_publisher_operation(&headers, peer, &publisher, McpCost::Upload,)
    );
}

#[test]
fn verified_viewers_share_a_nat_but_not_each_others_budget_or_raw_headers() {
    let mut config = AppConfig::default();
    config.ingress.verified_viewers_per_window = 1;
    let state = IngressState::from_config(&config);
    let mut first_headers = HeaderMap::new();
    first_headers.insert(
        VERIFIED_SOURCE_HEADER,
        HeaderValue::from_static("0123456789abcdef01234567"),
    );
    first_headers.insert(header::COOKIE, HeaderValue::from_static("session=one"));
    let mut alternate_headers = first_headers.clone();
    alternate_headers.insert(header::COOKIE, HeaderValue::from_static("session=rotated"));
    let alice = crate::model::Viewer {
        email: Some(crate::model::EmailAddress("Alice@Example.test".to_owned())),
        org: Some(crate::model::OrgId("acme".to_owned())),
        is_admin: false,
    };
    let bob = crate::model::Viewer {
        email: Some(crate::model::EmailAddress("bob@example.test".to_owned())),
        org: Some(crate::model::OrgId("acme".to_owned())),
        is_admin: false,
    };
    assert!(state.allow_verified_viewer(&first_headers, &alice, ViewerCost::Read));
    assert!(!state.allow_verified_viewer(&alternate_headers, &alice, ViewerCost::Read));
    assert!(state.allow_verified_viewer(&alternate_headers, &bob, ViewerCost::Read));
}

#[test]
fn verified_share_budget_uses_the_canonical_resolved_token() {
    let mut config = AppConfig::default();
    config.ingress.shares_per_window = 1;
    let state = IngressState::from_config(&config);
    let mut headers = HeaderMap::new();
    headers.insert(
        VERIFIED_SOURCE_HEADER,
        HeaderValue::from_static("0123456789abcdef01234567"),
    );
    let token = crate::model::ShareToken("sharetoken".to_owned());
    assert!(state.allow_verified_share(&headers, &token));
    assert!(!state.allow_verified_share(&headers, &token));
}

#[test]
fn high_cardinality_sources_evict_in_constant_bounded_state() {
    let config = AppConfig::default();
    let state = IngressState::from_config(&config);
    for index in 0..(MAX_BUCKETS + 128) {
        assert!(state.allow("load", 1, format!("source-{index}")));
    }
    assert!(
        state
            .buckets
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
            <= MAX_BUCKETS
    );
}

#[tokio::test]
async fn mcp_admission_rejections_keep_the_frozen_transport_envelopes() {
    let response = mcp_admission_response(StatusCode::TOO_MANY_REQUESTS, Some(60));
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(response.headers()[header::RETRY_AFTER], "60");
    let bytes = axum::body::to_bytes(response.into_body(), 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["jsonrpc"], "2.0");
    assert!(json["id"].is_null());
    assert_eq!(json["error"]["data"]["reason"], "rate_limited");

    let oversized = mcp_admission_response(StatusCode::PAYLOAD_TOO_LARGE, None);
    let bytes = axum::body::to_bytes(oversized.into_body(), 1024)
        .await
        .unwrap();
    assert_eq!(bytes.as_ref(), br#"{"error":"payload too large"}"#);

    let unsupported = mcp_admission_response(StatusCode::UNSUPPORTED_MEDIA_TYPE, None);
    let bytes = axum::body::to_bytes(unsupported.into_body(), 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["error"]["data"]["reason"], "unsupported_media_type");

    let malformed = mcp_admission_response(StatusCode::BAD_REQUEST, None);
    let bytes = axum::body::to_bytes(malformed.into_body(), 1024)
        .await
        .unwrap();
    assert_eq!(bytes.as_ref(), br#"{"error":"invalid JSON"}"#);
}

#[tokio::test]
async fn concurrency_rejects_pressure_and_recovers_after_a_request_completes() {
    let mut config = AppConfig::default();
    config.ingress.max_concurrent_requests = 1;
    let state = Arc::new(IngressState::from_config(&config));
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let first_call = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let app = Router::new()
        .route(
            "/slow",
            get({
                let started = Arc::clone(&started);
                let release = Arc::clone(&release);
                let first_call = Arc::clone(&first_call);
                move || {
                    let started = Arc::clone(&started);
                    let release = Arc::clone(&release);
                    let first_call = Arc::clone(&first_call);
                    async move {
                        if first_call.swap(false, std::sync::atomic::Ordering::SeqCst) {
                            started.notify_one();
                            release.notified().await;
                        }
                        "ok"
                    }
                }
            }),
        )
        .layer(middleware::from_fn_with_state(state, admit));
    let task = tokio::spawn({
        let app = app.clone();
        async move {
            app.oneshot(Request::builder().uri("/slow").body(Body::empty()).unwrap())
                .await
                .unwrap()
        }
    });
    started.notified().await;
    let pressured = app
        .clone()
        .oneshot(Request::builder().uri("/slow").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(pressured.status(), StatusCode::SERVICE_UNAVAILABLE);
    release.notify_one();
    assert_eq!(task.await.unwrap().status(), StatusCode::OK);
    let recovered = app
        .oneshot(Request::builder().uri("/slow").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(recovered.status(), StatusCode::OK);
}

#[tokio::test]
async fn chunked_body_limit_and_deadline_are_bounded() {
    let body = Body::new(ChunkedBody {
        chunks: VecDeque::from([
            axum::body::Bytes::from_static(b"12"),
            axum::body::Bytes::from_static(b"345"),
        ]),
    });
    assert_eq!(
        read_body_limited(body, 4, Duration::from_secs(1)).await,
        Err(BodyReadError::TooLarge)
    );
}

#[tokio::test]
async fn discard_only_drain_consumes_oversized_frames_without_a_parser_buffer() {
    let body = Body::new(ChunkedBody {
        chunks: VecDeque::from([
            axum::body::Bytes::from(vec![b'x'; 8 * 1024]),
            axum::body::Bytes::from(vec![b'y'; 8 * 1024]),
        ]),
    });
    assert_eq!(
        discard_body_bounded(body, Duration::from_secs(1)).await,
        Ok(())
    );
}

#[tokio::test]
async fn slow_body_is_timed_out_without_entering_a_handler() {
    let result = read_body_limited(Body::new(StalledBody), 16, Duration::from_millis(1)).await;
    assert_eq!(result, Err(BodyReadError::Timeout));
}

#[tokio::test]
async fn slow_drip_cannot_extend_the_whole_body_deadline_per_frame() {
    let body = Body::new(SlowDripBody {
        chunks: VecDeque::from([
            axum::body::Bytes::from_static(b"a"),
            axum::body::Bytes::from_static(b"b"),
            axum::body::Bytes::from_static(b"c"),
        ]),
        delay: Duration::from_millis(20),
        wait: None,
    });
    assert_eq!(
        read_body_limited(body, 16, Duration::from_millis(35)).await,
        Err(BodyReadError::Timeout)
    );
}

#[test]
fn deep_and_wide_json_are_rejected() {
    let mut config = AppConfig::default();
    config.ingress.json_max_depth = 2;
    config.ingress.json_max_members = 1;
    assert_eq!(
        validate_json_complexity(
            &OrderedJson::Array(vec![OrderedJson::Array(vec![OrderedJson::Null])]),
            &config.ingress
        ),
        Err("json_too_deep")
    );
    assert_eq!(
        validate_json_complexity(
            &OrderedJson::Array(vec![OrderedJson::Null, OrderedJson::Null]),
            &config.ingress
        ),
        Err("json_too_wide")
    );
}

#[test]
fn untrusted_forwarded_headers_do_not_choose_the_source() {
    let config = IngressConfig::default();
    let mut headers = HeaderMap::new();
    headers.insert("x-forwarded-for", HeaderValue::from_static("203.0.113.7"));
    assert_eq!(
        trusted_source(&headers, Some("127.0.0.1:1234".parse().unwrap()), &config),
        "127.0.0.1"
    );
}

#[test]
fn only_a_configured_proxy_can_supply_cf_connecting_ip() {
    let config = IngressConfig {
        trusted_proxy_cidrs: vec!["127.0.0.0/8".parse().unwrap()],
        ..IngressConfig::default()
    };
    let mut headers = HeaderMap::new();
    headers.insert("cf-connecting-ip", HeaderValue::from_static("203.0.113.7"));
    headers.insert("x-forwarded-for", HeaderValue::from_static("198.51.100.8"));
    assert_eq!(
        trusted_source(&headers, Some("127.0.0.1:1234".parse().unwrap()), &config),
        "203.0.113.7"
    );
    assert_eq!(
        trusted_source(&headers, Some("192.0.2.9:1234".parse().unwrap()), &config),
        "192.0.2.9"
    );
}
