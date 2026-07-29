//! U05 — Cloudflare Access session retry (`src/security/access_retry.rs`).
//!
//! Mirrors `test/identity.test.js:136-217`, which is the Node suite that froze this behaviour.
//! The Node-parity half is in `u05_node_parity.rs`.

use artifact_mcp::config::AccessIdentityMode;
use artifact_mcp::security::access_retry::{
    ACCESS_RETRY_PARAM, access_retry_target, is_retryable_path,
};
use artifact_mcp::security::identity::ACCESS_JWT_HEADER;
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, Uri};

fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
    let mut map = HeaderMap::new();
    for (name, value) in pairs {
        map.append(
            HeaderName::from_bytes(name.as_bytes()).expect("test header name"),
            HeaderValue::from_str(value).expect("test header value"),
        );
    }
    map
}

/// A signed-in `GET` with the cookie but no assertion — the exact state the retry exists for.
fn target(url: &str) -> Option<String> {
    target_with(
        &Method::GET,
        url,
        &[("cookie", "CF_Authorization=session-token")],
    )
}

fn target_with(method: &Method, url: &str, pairs: &[(&str, &str)]) -> Option<String> {
    access_retry_target(
        method,
        &url.parse::<Uri>().expect("test URI"),
        &headers(pairs),
        AccessIdentityMode::Jwt,
        ACCESS_RETRY_PARAM,
    )
}

#[test]
fn the_gallery_and_a_direct_artifact_navigation_are_retried() {
    assert_eq!(target("/").as_deref(), Some("/?cf_access_retry=1"));
    assert_eq!(
        target("/abcdef123456").as_deref(),
        Some("/abcdef123456?cf_access_retry=1")
    );
    assert_eq!(
        target("/abcdef123456?view=grid").as_deref(),
        Some("/abcdef123456?view=grid&cf_access_retry=1")
    );
}

#[test]
fn non_shell_reserved_malformed_and_multi_segment_paths_are_never_retried() {
    for url in [
        "/raw/abcdef123456",
        "/raw/abcdef123456/x.css",
        "/thumbnails/abcdef123456",
        "/s/sometoken",
        "/abcdef123456/history",
        "/abcdef123456/feedback",
        "/mcp",
        "/health",
        "/settings",
        "/raw",
        "/short",
        "/aaaaaaaaaaaaaaaaaaaaaaaaa",
        "/ABCDEF123456",
        "/abcdef/second",
    ] {
        assert_eq!(target(url), None, "{url}");
    }
}

#[test]
fn every_guard_is_load_bearing() {
    const URL: &str = "/abcdef123456";
    let cookie = ("cookie", "CF_Authorization=session-token");

    // No cookie: the visitor is simply signed out, and a reload would not help.
    assert_eq!(target_with(&Method::GET, URL, &[]), None);
    // An assertion is present, so verification already had everything it needed.
    assert_eq!(
        target_with(
            &Method::GET,
            URL,
            &[cookie, (ACCESS_JWT_HEADER, "signed-assertion")]
        ),
        None
    );
    // An *empty* assertion header is falsy in Node, so the retry still applies.
    assert!(
        target_with(&Method::GET, URL, &[cookie, (ACCESS_JWT_HEADER, "")]).is_some(),
        "an empty assertion header must not suppress the retry"
    );
    // Already retried once.
    assert_eq!(
        target_with(&Method::GET, "/abcdef123456?cf_access_retry=1", &[cookie]),
        None
    );
    // The value does not matter, only the parameter's presence.
    assert_eq!(
        target_with(&Method::GET, "/abcdef123456?cf_access_retry=0", &[cookie]),
        None
    );
    // A retry must never replay a mutation.
    for method in [Method::POST, Method::PUT, Method::DELETE, Method::HEAD] {
        assert_eq!(target_with(&method, URL, &[cookie]), None, "{method}");
    }
    // Only `jwt` mode has an assertion to be missing.
    for mode in [
        AccessIdentityMode::HeaderTrust,
        AccessIdentityMode::Disabled,
    ] {
        assert_eq!(
            access_retry_target(
                &Method::GET,
                &URL.parse::<Uri>().expect("test URI"),
                &headers(&[cookie]),
                mode,
                ACCESS_RETRY_PARAM,
            ),
            None,
            "{mode}"
        );
    }
}

#[test]
fn the_returned_target_is_always_a_local_path() {
    // A protocol-relative request target resolves against the synthetic origin, and only the
    // path and query are returned, so it cannot become an open redirect.
    assert_eq!(
        target("//evil.example/abcdef123456").as_deref(),
        Some("/abcdef123456?cf_access_retry=1")
    );
    assert_eq!(
        target("//evil.example/"),
        Some("/?cf_access_retry=1".to_owned())
    );
}

#[test]
fn the_existing_query_is_re_serialized_the_way_url_search_params_does() {
    // `searchParams.set` rebuilds the whole query, so `%20` comes back as `+`.
    assert_eq!(
        target("/abcdef123456?q=a%20b").as_deref(),
        Some("/abcdef123456?q=a+b&cf_access_retry=1")
    );
    // A bare flag keeps its empty value.
    assert_eq!(
        target("/abcdef123456?flag").as_deref(),
        Some("/abcdef123456?flag=&cf_access_retry=1")
    );
    // Repeated parameters are preserved in order.
    assert_eq!(
        target("/abcdef123456?a=1&a=2").as_deref(),
        Some("/abcdef123456?a=1&a=2&cf_access_retry=1")
    );
}

#[test]
fn path_eligibility_matches_the_reserved_id_rules() {
    assert!(is_retryable_path("/"));
    assert!(is_retryable_path("/abcdef123456"));
    assert!(is_retryable_path("/abcdef"));
    // Reserved ids and anything the id validator rejects.
    for path in [
        "/mcp",
        "/health",
        "/settings",
        "/raw",
        "/s",
        "/favicon.ico",
        "/robots.txt",
        "/abcde",
        "/ABCDEF",
        "/abc-def123",
        "/a/b",
        "//",
        "relative",
    ] {
        assert!(!is_retryable_path(path), "{path}");
    }
    // The empty id, which Node covers via the `""` member of its RESERVED set.
    assert!(!is_retryable_path(""));
}
