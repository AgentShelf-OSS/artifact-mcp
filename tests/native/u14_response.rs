use artifact_mcp::{
    config::{AppConfig, MapEnv},
    http::{
        artifact_response::{
            ANCHOR_BRIDGE, ArtifactResponseOptions, DOCUMENT_SANDBOX, RawCachePolicy,
            artifact_response, download_name, inject_anchor_bridge, inject_data_client,
            is_html_content_type, raw_artifact_headers, strip_scripts,
        },
        middleware::{append_no_transform, mcp_body_limit, prevent_response_transforms, weak_etag},
    },
    model::ArtifactFile,
};
use axum::{
    Json, Router,
    body::{Body, to_bytes},
    http::{HeaderMap, Request, StatusCode, header},
    middleware,
    routing::{get, post},
};
use serde_json::Value;
use tower::ServiceExt;

fn file(content_type: &str, content: &[u8]) -> ArtifactFile {
    ArtifactFile {
        content: content.to_vec(),
        content_type: content_type.to_owned(),
    }
}

#[test]
fn every_raw_content_type_and_download_keeps_the_opaque_origin_sandbox() {
    assert_eq!(
        DOCUMENT_SANDBOX,
        "sandbox allow-scripts allow-popups allow-forms allow-modals; default-src 'none'; connect-src 'none'; script-src 'self' 'unsafe-inline' https://cdn.jsdelivr.net; style-src 'self' 'unsafe-inline' https://fonts.googleapis.com; font-src 'self' data: blob: https://fonts.gstatic.com; img-src 'self' data: blob:; media-src 'self' data: blob:; worker-src 'self' blob:"
    );
    for content_type in [
        "text/html; charset=utf-8",
        "image/svg+xml",
        "application/xml",
        "text/css; charset=utf-8",
        "image/png",
        "application/octet-stream",
    ] {
        let headers = raw_artifact_headers(content_type, None).expect("trusted U07 MIME");
        assert_eq!(headers[header::CONTENT_SECURITY_POLICY], DOCUMENT_SANDBOX);
        assert!(!DOCUMENT_SANDBOX.contains("allow-same-origin"));
        assert_eq!(headers[header::CONTENT_TYPE], content_type);
        assert_eq!(headers["x-content-type-options"], "nosniff");
        assert_eq!(headers["referrer-policy"], "no-referrer");
        assert_eq!(headers[header::CACHE_CONTROL], "private, max-age=60");
        assert!(!headers.contains_key("content-security-policy-report-only"));
        assert!(!headers.contains_key("cross-origin-embedder-policy"));
    }

    let name = download_name("  Quarterly 🔥 report  ");
    assert_eq!(name, "Quarterly-report.html");
    let headers = raw_artifact_headers("text/html; charset=utf-8", Some(&name))
        .expect("sanitized attachment name");
    assert_eq!(
        headers[header::CONTENT_DISPOSITION],
        "attachment; filename=\"Quarterly-report.html\""
    );
    assert_eq!(headers[header::CONTENT_SECURITY_POLICY], DOCUMENT_SANDBOX);
}

#[test]
fn anchor_and_preview_transforms_match_the_node_order() {
    let original = b"<body><script>const fake='</body>'</script><p>ok</p></body>";
    let bridged = inject_anchor_bridge(original, Some("pages/<one>/$&.html"));
    let bridged_text = String::from_utf8(bridged.clone()).expect("bridge remains utf8");
    assert_eq!(bridged_text.matches("artifact-anchor-bridge").count(), 1);
    assert!(bridged_text.find("artifact-anchor-bridge") < bridged_text.rfind("</body>"));
    assert!(bridged_text.contains("\\u003cone>"));

    let stripped = strip_scripts(&bridged);
    assert_eq!(stripped, b"<body><p>ok</p></body>");
    assert_eq!(ANCHOR_BRIDGE.len(), 13_187);
    assert!(ANCHOR_BRIDGE.contains("type:\"anchor:navigate\""));
    assert!(ANCHOR_BRIDGE.contains("url.protocol===\"http:\"||url.protocol===\"https:\""));
    assert!(!ANCHOR_BRIDGE.contains("allow-popups-to-escape-sandbox"));
}

#[test]
fn raw_anchor_bridge_golden_freezes_the_current_injected_bridge_bytes() {
    let golden: Value = serde_json::from_str(include_str!(
        "../../conformance/goldens/raw.anchor-bridge.json"
    ))
    .expect("valid raw anchor-bridge golden");
    let body = String::from_utf8(inject_data_client(&inject_anchor_bridge(
        b"<!doctype html><html><body><p>anchor me</p></body></html>",
        None,
    )))
    .expect("injected bridge remains UTF-8");

    assert_eq!(golden["steps"][2]["body"]["data"], body);
    assert_eq!(
        golden["steps"][2]["headers"]["headers"]["etag"],
        weak_etag(body.as_bytes())
    );
}

#[tokio::test]
async fn raw_and_public_share_responses_receive_final_cache_policies() {
    async fn private() -> axum::response::Response {
        artifact_response(
            file("image/svg+xml", b"<svg><script>alert(1)</script></svg>"),
            ArtifactResponseOptions::default(),
        )
        .expect("raw response")
    }
    async fn public() -> axum::response::Response {
        artifact_response(
            file("application/xml", b"<root/ >"),
            ArtifactResponseOptions {
                cache: RawCachePolicy::PublicShare,
                ..ArtifactResponseOptions::default()
            },
        )
        .expect("share response")
    }
    let app = Router::new()
        .route("/private", get(private))
        .route("/public", get(public))
        .layer(middleware::from_fn(prevent_response_transforms));

    let private = app
        .clone()
        .oneshot(
            Request::get("/private")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(
        private.headers()[header::CACHE_CONTROL],
        "private, max-age=60, no-transform"
    );
    assert_eq!(
        private.headers()[header::CONTENT_SECURITY_POLICY],
        DOCUMENT_SANDBOX
    );

    let public = app
        .oneshot(
            Request::get("/public")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(
        public.headers()[header::CACHE_CONTROL],
        "no-store, no-transform"
    );
    assert_eq!(public.headers()["x-robots-tag"], "noindex");
    assert_eq!(
        public.headers()[header::CONTENT_SECURITY_POLICY],
        DOCUMENT_SANDBOX
    );

    let mut already = HeaderMap::new();
    already.insert(
        header::CACHE_CONTROL,
        "No-Store, NO-TRANSFORM".parse().expect("header"),
    );
    append_no_transform(&mut already);
    assert_eq!(already[header::CACHE_CONTROL], "No-Store, NO-TRANSFORM");
}

#[tokio::test]
async fn mcp_body_limit_accepts_the_exact_configured_boundary_above_two_megabytes() {
    async fn json(Json(_value): Json<Value>) -> StatusCode {
        StatusCode::OK
    }

    const LIMIT: usize = 8 * 1024 * 1024;
    let config = AppConfig::from_source(&MapEnv::empty().with("MCP_JSON_LIMIT", "8mb"))
        .expect("valid body limit");
    let app = Router::new().route("/mcp", post(json).layer(mcp_body_limit(&config)));

    fn body_of_size(size: usize) -> Body {
        let envelope = "{\"data\":\"\"}";
        let data = "x".repeat(size - envelope.len());
        Body::from(format!("{{\"data\":\"{data}\"}}"))
    }

    let accepted = app
        .clone()
        .oneshot(
            Request::post("/mcp")
                .header(header::CONTENT_TYPE, "application/json")
                .body(body_of_size(LIMIT))
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(accepted.status(), StatusCode::OK);

    let rejected = app
        .oneshot(
            Request::post("/mcp")
                .header(header::CONTENT_TYPE, "application/json")
                .body(body_of_size(LIMIT + 1))
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(rejected.status(), StatusCode::PAYLOAD_TOO_LARGE);
    let body = to_bytes(rejected.into_body(), 1024)
        .await
        .expect("rejection body");
    assert!(!body.is_empty());
}

#[tokio::test]
async fn native_reader_is_explicit_html_only_and_keeps_sandbox_policy() {
    for (reader, preview, download, content_type, expected) in [
        (false, false, false, "text/html", false),
        (true, false, false, "text/html", true),
        (true, true, false, "text/html", false),
        (true, false, true, "text/html", false),
        (true, false, false, "image/svg+xml", false),
    ] {
        let response = artifact_response(
            file(content_type, b"<body><p>Readable text</p></body>"),
            ArtifactResponseOptions {
                reader,
                preview,
                download_title: download.then_some("book"),
                page_path: Some("chapters/one.html"),
                ..ArtifactResponseOptions::default()
            },
        )
        .unwrap();
        assert_eq!(
            response.headers()[header::CONTENT_SECURITY_POLICY],
            DOCUMENT_SANDBOX
        );
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let text = String::from_utf8(bytes.to_vec()).unwrap();
        assert_eq!(text.contains("artifact-reader-bridge"), expected);
        if expected {
            assert!(text.contains("reader:next"));
            assert!(text.contains("__artifactEreader"));
            assert!(text.ends_with("</script></body>"));
        }
    }
}

// ---------------------------------------------------------------------------
// Frozen vectors from the retired Node runtime (`lib/artifact-http.js`)
// ---------------------------------------------------------------------------

#[test]
fn anchor_bridge_injection_matches_the_frozen_node_output() {
    // `(content, pagePath, output with the bridge shown as {BRIDGE}, JS literal for the page)`.
    // The last `</body>` wins, U+FEFF counts as whitespace in `</body >`, and the page path is a
    // JSON string with `<` escaped.
    // Frozen vector: captured from the retired Node runtime at commit 7211de0.
    let cases: [(&str, Option<&str>, &str, &str); 5] = [
        (
            "<!doctype html><html><body><p>anchor me</p></body></html>",
            None,
            "<!doctype html><html><body><p>anchor me</p>{BRIDGE}</body></html>",
            "null",
        ),
        (
            "<p>no body tag</p>",
            None,
            "<p>no body tag</p>{BRIDGE}",
            "null",
        ),
        (
            "<BODY class=x>upper</BODY\u{feff}>",
            None,
            "<BODY class=x>upper{BRIDGE}</BODY\u{feff}>",
            "null",
        ),
        (
            "<body><script>const fake='</body>'</script><!-- </body> --><p>x</p></body>",
            Some("deep/page.html"),
            "<body><script>const fake='</body>'</script><!-- </body> --><p>x</p>{BRIDGE}</body>",
            r#""deep/page.html""#,
        ),
        (
            "<body>x</body>",
            Some("quote\"/<tag>/\u{2028}.html"),
            "<body>x{BRIDGE}</body>",
            "\"quote\\\"/\\u003ctag>/\u{2028}.html\"",
        ),
    ];
    let marker = "var d=document,w=window,page=null";
    assert_eq!(ANCHOR_BRIDGE.matches(marker).count(), 1);
    for (content, page_path, template, page) in cases {
        let bridge = ANCHOR_BRIDGE.replace(marker, &format!("var d=document,w=window,page={page}"));
        let injected = String::from_utf8(inject_anchor_bridge(content.as_bytes(), page_path))
            .expect("bridge output is UTF-8");
        assert_eq!(
            injected,
            template.replace("{BRIDGE}", &bridge),
            "{content:?} / {page_path:?}"
        );
    }

    // `$` in a page path is data. The retired Node runtime spliced the literal in with
    // `String.prototype.replace`, which expanded `$&`, `$$`, `` $` `` and `$'` and could paste
    // bridge source into the page string. The page literal must stay exactly the JSON encoding.
    let (before, after) = ANCHOR_BRIDGE.split_once(marker).expect("marker");
    let injected = String::from_utf8(inject_anchor_bridge(
        b"<body>x</body>",
        Some("cash/$&/$$/$`/$'.html"),
    ))
    .expect("bridge output is UTF-8");
    assert_eq!(
        injected,
        format!(
            "<body>x{before}var d=document,w=window,page=\"cash/$&/$$/$`/$'.html\"{after}</body>"
        )
    );
}

#[test]
fn script_stripping_and_html_detection_match_the_frozen_node_output() {
    // JavaScript's `\s` includes U+FEFF but not U+0085, so only the first closes the tag.
    // Frozen vector: captured from the retired Node runtime at commit 7211de0.
    for (input, expected) in [
        ("<p>plain</p>", "<p>plain</p>"),
        ("<script>alert(1)</script><p>after</p>", "<p>after</p>"),
        ("<SCRIPT type=x>one</ScRiPt><script>two</script>", ""),
        (
            "<scriptx>keep</script><script>drop</script>",
            "<scriptx>keep</script>",
        ),
        ("<script>unclosed", "<script>unclosed"),
        ("a<script>b</script\u{feff}>c", "ac"),
        ("a<script>b</script\u{85}>c", "a<script>b</script\u{85}>c"),
        ("<script>first<script>nested</script>tail", "tail"),
    ] {
        assert_eq!(
            String::from_utf8(strip_scripts(input.as_bytes())).expect("UTF-8"),
            expected,
            "{input:?}"
        );
    }

    // Frozen vector: captured from the retired Node runtime at commit 7211de0.
    for (content_type, html) in [
        ("text/html", true),
        ("text/html; charset=utf-8", true),
        ("TEXT/HTML;CHARSET=UTF-8", true),
        ("text/htmlx", false),
        (" text/html", false),
        ("image/svg+xml", false),
        ("application/xml", false),
        ("text/css; charset=utf-8", false),
        ("image/png", false),
        ("application/octet-stream", false),
    ] {
        assert_eq!(is_html_content_type(content_type), html, "{content_type:?}");
        let headers = raw_artifact_headers(content_type, None).expect("trusted MIME");
        assert_eq!(headers.len(), 5, "{content_type:?}");
        assert_eq!(headers[header::CONTENT_TYPE], content_type);
        assert_eq!(headers[header::CONTENT_SECURITY_POLICY], DOCUMENT_SANDBOX);
        assert_eq!(headers["x-content-type-options"], "nosniff");
        assert_eq!(headers["referrer-policy"], "no-referrer");
        assert_eq!(headers[header::CACHE_CONTROL], "private, max-age=60");
    }
}

#[test]
fn download_names_match_the_frozen_node_output() {
    // `(title, download name)`; the Content-Disposition header quotes the name verbatim.
    // Frozen vector: captured from the retired Node runtime at commit 7211de0.
    for (title, expected) in [
        ("", "artifact.html"),
        ("Raw Case", "Raw-Case.html"),
        ("  Quarterly 🔥 report  ", "Quarterly-report.html"),
        ("---", "artifact.html"),
        ("a - b", "a---b.html"),
        ("résumé.2026", "r-sum-.2026.html"),
        ("safe_name-v2.1", "safe_name-v2.1.html"),
        (
            "1234567890abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ.--------------------tail",
            "1234567890abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ.-----------------.html",
        ),
    ] {
        let name = download_name(title);
        assert_eq!(name, expected, "{title:?}");
        let headers =
            raw_artifact_headers("text/html; charset=utf-8", Some(&name)).expect("safe name");
        assert_eq!(
            headers[header::CONTENT_DISPOSITION],
            format!("attachment; filename=\"{expected}\"").as_str()
        );
    }
}
