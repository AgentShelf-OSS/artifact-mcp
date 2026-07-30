//! Request-authenticity matrix for cookie-authenticated portal mutations (PBI-032).

use std::sync::Arc;

use artifact_mcp::{
    config::AppConfig,
    http::middleware::{
        PORTAL_MUTATION_HEADER, RequestAuthenticityState, same_origin_gate, weak_etag,
    },
};
use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode, header},
    middleware,
    routing::post,
};
use tower::ServiceExt;

fn app() -> Router {
    Router::new()
        .route(
            "/portal",
            post(|| async { "mutated" }).get(|| async { "read" }),
        )
        .route("/mcp", post(|| async { "machine" }))
        .layer(middleware::from_fn_with_state(
            Arc::new(RequestAuthenticityState::from_config(&AppConfig::default())),
            same_origin_gate,
        ))
}

fn portal_request() -> axum::http::request::Builder {
    Request::builder().method("POST").uri("/portal").header(
        header::COOKIE,
        "CF_Authorization=authenticated-viewer-session",
    )
}

#[tokio::test]
async fn cross_site_and_sandboxed_portal_posts_are_rejected_before_the_handler() {
    const FORBIDDEN: &[u8] = br#"{"error":"forbidden","code":"same_origin_required"}"#;
    for (fetch_site, origin) in [
        ("cross-site", "https://evil.example"),
        ("cross-site", "null"),
        ("same-site", "http://localhost:3480"),
    ] {
        let response = app()
            .oneshot(
                portal_request()
                    .header("sec-fetch-site", fetch_site)
                    .header(header::ORIGIN, origin)
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_eq!(
            response.headers().get(header::CONTENT_TYPE).unwrap(),
            "application/json; charset=utf-8"
        );
        assert_eq!(
            response
                .headers()
                .get(header::ETAG)
                .and_then(|value| value.to_str().ok()),
            Some(weak_etag(FORBIDDEN).as_str())
        );
        assert_eq!(
            response
                .headers()
                .get(header::VARY)
                .and_then(|value| value.to_str().ok()),
            Some("Sec-Fetch-Site, Origin")
        );
        assert_eq!(
            to_bytes(response.into_body(), usize::MAX)
                .await
                .expect("forbidden response body"),
            FORBIDDEN
        );
    }
}

#[tokio::test]
async fn portal_header_and_same_origin_fetch_metadata_allow_a_mutation() {
    let response = app()
        .oneshot(
            portal_request()
                .header(PORTAL_MUTATION_HEADER, "1")
                .header("sec-fetch-site", "same-origin")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK);

    let response = app()
        .oneshot(
            portal_request()
                .header("sec-fetch-site", "same-origin")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn trusted_access_email_cannot_bypass_the_same_portal_gate() {
    let denied = app()
        .oneshot(
            Request::post("/portal")
                .header("cf-access-authenticated-user-email", "viewer@example.test")
                .header("sec-fetch-site", "same-origin")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(denied.status(), StatusCode::FORBIDDEN);

    let allowed = app()
        .oneshot(
            Request::post("/portal")
                .header("cf-access-authenticated-user-email", "viewer@example.test")
                .header(PORTAL_MUTATION_HEADER, "1")
                .header("sec-fetch-site", "same-origin")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(allowed.status(), StatusCode::OK);
}

#[tokio::test]
async fn origin_fallback_is_canonical_and_fetch_none_is_never_accepted() {
    for (headers, expected) in [
        (
            vec![
                (PORTAL_MUTATION_HEADER, "1"),
                ("origin", "http://localhost:3480"),
            ],
            StatusCode::OK,
        ),
        (
            vec![
                (PORTAL_MUTATION_HEADER, "1"),
                ("origin", "http://evil.example"),
            ],
            StatusCode::FORBIDDEN,
        ),
        (
            vec![(PORTAL_MUTATION_HEADER, "1"), ("origin", "not an origin")],
            StatusCode::FORBIDDEN,
        ),
        (
            vec![(PORTAL_MUTATION_HEADER, "1"), ("sec-fetch-site", "none")],
            StatusCode::FORBIDDEN,
        ),
    ] {
        let mut request = portal_request();
        for (name, value) in headers {
            request = request.header(name, value);
        }
        let response = app()
            .oneshot(request.body(Body::empty()).expect("request"))
            .await
            .expect("response");
        assert_eq!(response.status(), expected);
    }

    let response = app()
        .oneshot(
            portal_request()
                .header(PORTAL_MUTATION_HEADER, "1")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn mcp_and_get_are_not_subject_to_the_portal_mutation_gate() {
    let mcp = app()
        .oneshot(Request::post("/mcp").body(Body::empty()).expect("request"))
        .await
        .expect("response");
    assert_eq!(mcp.status(), StatusCode::OK);

    let get = app()
        .oneshot(
            Request::get("/portal")
                .header(header::COOKIE, "CF_Authorization=viewer")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(get.status(), StatusCode::OK);
}
