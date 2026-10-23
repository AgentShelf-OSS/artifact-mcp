// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
//! The HTTP contract of the Node renderer, tested against the running `artifact-preview` binary.

mod support;

use std::io::{Read, Write};
use std::time::Duration;

use artifact_preview::client::{HttpResponse, send, send_with_authorization};
use artifact_preview::image_info::png_dimensions;
use hyper::body::Bytes;
use hyper::{Method, StatusCode};
use support::{Service, chrome, scratch_dir};

const TIMEOUT: Duration = Duration::from_secs(20);

/// A valid shared token (32 to 4096 printable ASCII characters).
const TOKEN: &str = "k3y_0123456789-abcdefghij.ABCDEFGHIJ~xyz";

async fn post(service: &Service, body: impl Into<Bytes>) -> HttpResponse {
    send(
        &service.authority,
        Method::POST,
        "/render",
        Some(("application/json", body.into())),
        TIMEOUT,
    )
    .await
    .unwrap()
}

async fn render(service: &Service, body: serde_json::Value) -> (u32, u32) {
    let response = post(service, serde_json::to_vec(&body).unwrap()).await;
    assert_eq!(response.status, StatusCode::OK, "body {body}");
    assert_eq!(header(&response, "content-type"), "image/png");
    assert_common_headers(&response);
    png_dimensions(&response.body).expect("a PNG body")
}

fn header<'a>(response: &'a HttpResponse, name: &str) -> &'a str {
    response
        .headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
}

/// Every response carries these, as in `server.js`'s `send()`.
fn assert_common_headers(response: &HttpResponse) {
    assert_eq!(header(response, "cache-control"), "no-store");
    assert_eq!(header(response, "x-content-type-options"), "nosniff");
    assert_eq!(
        header(response, "content-length"),
        response.body.len().to_string()
    );
}

fn assert_json(response: &HttpResponse, status: StatusCode, body: &str) {
    assert_eq!(response.status, status);
    assert_eq!(
        header(response, "content-type"),
        "application/json; charset=utf-8"
    );
    assert_eq!(std::str::from_utf8(&response.body).unwrap(), body);
    assert_common_headers(response);
}

#[tokio::test]
async fn health_reports_ok() {
    let Some(chrome) = chrome() else { return };
    let service = Service::start(&chrome, "health", &[]);
    let response = send(&service.authority, Method::GET, "/health", None, TIMEOUT)
        .await
        .unwrap();
    assert_json(&response, StatusCode::OK, r#"{"status":"ok"}"#);
}

#[tokio::test]
async fn renders_at_twice_the_requested_size() {
    let Some(chrome) = chrome() else { return };
    let service = Service::start(&chrome, "sizes", &[]);
    let html = "<!doctype html><h1>size</h1>";
    assert_eq!(
        render(&service, serde_json::json!({"html": html})).await,
        (2400, 1260),
        "default 1200x630"
    );
    assert_eq!(
        render(
            &service,
            serde_json::json!({"html": html, "width": 800, "height": 600})
        )
        .await,
        (1600, 1200)
    );
    assert_eq!(
        render(
            &service,
            serde_json::json!({"html": html, "width": 320, "height": 200})
        )
        .await,
        (640, 400)
    );
}

#[tokio::test]
async fn clamps_dimensions_with_node_number_rules() {
    let Some(chrome) = chrome() else { return };
    let service = Service::start(&chrome, "clamp", &[]);
    let html = "<p>clamp</p>";
    let cases = [
        (
            serde_json::json!({"html": html, "width": 5000, "height": 5000}),
            (3840, 2160),
        ),
        (
            serde_json::json!({"html": html, "width": 0, "height": -5}),
            (2400, 1260),
        ),
        (
            serde_json::json!({"html": html, "width": 99.5, "height": null}),
            (2400, 1260),
        ),
        (
            serde_json::json!({"html": html, "width": "640", "height": [360]}),
            (1280, 720),
        ),
        (
            serde_json::json!({"html": html, "width": "0x280", "height": "3.6e2"}),
            (1280, 720),
        ),
        (
            serde_json::json!({"html": html, "width": true, "height": true}),
            (2, 2),
        ),
        (
            serde_json::json!({"html": html, "width": "abc", "height": {}}),
            (2400, 1260),
        ),
    ];
    for (body, expected) in cases {
        assert_eq!(
            render(&service, body.clone()).await,
            expected,
            "body {body}"
        );
    }
}

#[tokio::test]
async fn rejects_invalid_input() {
    let Some(chrome) = chrome() else { return };
    let service = Service::start(&chrome, "invalid", &[]);
    for body in [
        "",
        "{",
        "not json",
        "{\"html\": \"x\",}",
        "{\"html\": 1e400}",
    ] {
        assert_json(
            &post(&service, body).await,
            StatusCode::BAD_REQUEST,
            r#"{"error":"invalid JSON"}"#,
        );
    }
    for body in [
        "{}",
        "null",
        "[]",
        "\"<p>x</p>\"",
        "{\"html\": 5}",
        "{\"html\": null}",
        "{\"html\": [\"x\"]}",
    ] {
        assert_json(
            &post(&service, body).await,
            StatusCode::BAD_REQUEST,
            r#"{"error":"html must be a string"}"#,
        );
    }
    // `Number({toString: 1})` throws a TypeError in JavaScript; Node answered 500.
    let throwing = r#"{"html": "<p>x</p>", "width": {"toString": 1}}"#;
    assert_json(
        &post(&service, throwing).await,
        StatusCode::INTERNAL_SERVER_ERROR,
        r#"{"error":"render failed"}"#,
    );
}

#[tokio::test]
async fn unknown_routes_are_not_found() {
    let Some(chrome) = chrome() else { return };
    let service = Service::start(&chrome, "routes", &[]);
    for (method, path) in [
        (Method::GET, "/render"),
        (Method::PUT, "/render"),
        (Method::POST, "/health"),
        (Method::GET, "/health?x=1"),
        (Method::POST, "/render?x=1"),
        (Method::GET, "/"),
        (Method::GET, "/health/"),
    ] {
        let response = send(&service.authority, method.clone(), path, None, TIMEOUT)
            .await
            .unwrap();
        assert_json(&response, StatusCode::NOT_FOUND, r#"{"error":"not found"}"#);
    }
}

#[tokio::test]
async fn enforces_the_body_limit() {
    let Some(chrome) = chrome() else { return };
    let service = Service::start(&chrome, "limit", &[("MAX_BODY_BYTES", "1000")]);
    // Exactly at the limit is accepted.
    let prefix = r#"{"html":"<p>"#;
    let suffix = r#"</p>"}"#;
    let at_limit = format!(
        "{prefix}{}{suffix}",
        "x".repeat(1000 - prefix.len() - suffix.len())
    );
    assert_eq!(at_limit.len(), 1000);
    assert_eq!(post(&service, at_limit).await.status, StatusCode::OK);
    // One byte more is rejected.
    let over = format!(
        "{prefix}{}{suffix}",
        "x".repeat(1001 - prefix.len() - suffix.len())
    );
    assert_json(
        &post(&service, over).await,
        StatusCode::PAYLOAD_TOO_LARGE,
        r#"{"error":"request too large"}"#,
    );

    // A chunked body without content-length is counted as it streams in.
    let mut stream = std::net::TcpStream::connect(&service.authority).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let chunk = "y".repeat(800);
    write!(
        stream,
        "POST /render HTTP/1.1\r\nHost: test\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\n\r\n\
         {:x}\r\n{chunk}\r\n{:x}\r\n{chunk}\r\n0\r\n\r\n",
        chunk.len(),
        chunk.len()
    )
    .unwrap();
    let mut response = Vec::new();
    let _ = stream.read_to_end(&mut response);
    let response = String::from_utf8_lossy(&response);
    assert!(response.starts_with("HTTP/1.1 413"), "{response}");
    assert!(response.contains("cache-control: no-store"), "{response}");
    assert!(
        response.ends_with(r#"{"error":"request too large"}"#),
        "{response}"
    );
}

#[tokio::test]
async fn second_render_is_refused_while_one_is_in_flight() {
    let Some(chrome) = chrome() else { return };
    let service = Service::start(&chrome, "busy", &[]);
    let slow = serde_json::json!({"html": "<script>const t = Date.now(); while (Date.now() - t < 2000) {}</script><p>slow</p>"});
    let authority = service.authority.clone();
    let first = tokio::spawn(async move {
        let body = Bytes::from(serde_json::to_vec(&slow).unwrap());
        send(
            &authority,
            Method::POST,
            "/render",
            Some(("application/json", body)),
            TIMEOUT,
        )
        .await
        .unwrap()
    });
    tokio::time::sleep(Duration::from_millis(500)).await;
    let second = post(&service, r#"{"html": "<p>fast</p>"}"#).await;
    assert_json(
        &second,
        StatusCode::SERVICE_UNAVAILABLE,
        r#"{"error":"renderer busy"}"#,
    );
    // Health stays available during a render.
    let health = send(&service.authority, Method::GET, "/health", None, TIMEOUT)
        .await
        .unwrap();
    assert_eq!(health.status, StatusCode::OK);
    let first = first.await.unwrap();
    assert_eq!(first.status, StatusCode::OK);
    // The slot is free again.
    assert_eq!(
        post(&service, r#"{"html": "<p>after</p>"}"#).await.status,
        StatusCode::OK
    );
}

#[tokio::test]
async fn smoke_and_healthcheck_commands_work() {
    let Some(chrome) = chrome() else { return };
    let service = Service::start(&chrome, "commands", &[]);
    let port = service.authority.rsplit(':').next().unwrap().to_owned();
    let binary = env!("CARGO_BIN_EXE_artifact-preview");
    let health = std::process::Command::new(binary)
        .arg("healthcheck")
        .env_clear()
        .env("PORT", &port)
        .status()
        .unwrap();
    assert!(health.success());
    let smoke = std::process::Command::new(binary)
        .arg("smoke")
        .env_clear()
        .env(
            "PREVIEW_RENDERER_URL",
            format!("http://{}", service.authority),
        )
        .output()
        .unwrap();
    assert!(
        smoke.status.success(),
        "{}",
        String::from_utf8_lossy(&smoke.stderr)
    );
    assert!(
        String::from_utf8_lossy(&smoke.stdout).starts_with("preview renderer smoke test passed (")
    );
    drop(service);
    let down = std::process::Command::new(binary)
        .arg("healthcheck")
        .env_clear()
        .env("PORT", &port)
        .status()
        .unwrap();
    assert!(
        !down.success(),
        "healthcheck must fail when nothing listens"
    );
    let usage = std::process::Command::new(binary)
        .arg("bogus")
        .status()
        .unwrap();
    assert_eq!(usage.code(), Some(2));
}

async fn post_with(service: &Service, authorization: Option<&str>, html: &str) -> HttpResponse {
    let body = Bytes::from(serde_json::to_vec(&serde_json::json!({"html": html})).unwrap());
    send_with_authorization(
        &service.authority,
        Method::POST,
        "/render",
        Some(("application/json", body)),
        authorization,
        TIMEOUT,
    )
    .await
    .unwrap()
}

fn assert_unauthorized(response: &HttpResponse, case: &str) {
    assert_eq!(response.status, StatusCode::UNAUTHORIZED, "{case}");
    assert_json(
        response,
        StatusCode::UNAUTHORIZED,
        r#"{"error":"unauthorized"}"#,
    );
    assert_eq!(header(response, "www-authenticate"), "Bearer", "{case}");
}

/// Without a configured token the contract is unchanged: no header is needed, and an
/// `authorization` header is ignored, as the Node renderer ignored it. The server can therefore
/// start sending its token before the renderer requires one.
#[tokio::test]
async fn without_a_token_render_needs_no_authorization() {
    let Some(chrome) = chrome() else { return };
    let service = Service::start(&chrome, "no-token", &[("RENDER_TOKEN", "")]);
    let bearer = format!("Bearer {TOKEN}");
    for authorization in [None, Some(bearer.as_str()), Some("Bearer whatever")] {
        let response = post_with(&service, authorization, "<p>open</p>").await;
        assert_eq!(response.status, StatusCode::OK, "{authorization:?}");
        assert_eq!(header(&response, "content-type"), "image/png");
    }
}

#[tokio::test]
async fn a_configured_token_guards_render_but_not_health() {
    let Some(chrome) = chrome() else { return };
    let service = Service::start(&chrome, "token", &[("RENDER_TOKEN", TOKEN)]);
    let wrong_last = format!("Bearer {}X", &TOKEN[..TOKEN.len() - 1]);
    let lower_scheme = format!("bearer {TOKEN}");
    let upper_scheme = format!("BEARER {TOKEN}");
    let two_spaces = format!("Bearer  {TOKEN}");
    let bare = TOKEN.to_owned();
    let prefix = format!("Bearer {}", &TOKEN[..TOKEN.len() - 1]);
    let longer = format!("Bearer {TOKEN}x");
    let basic = "Basic dXNlcjpwYXNz".to_owned();
    let cases = [
        ("missing", None),
        (
            "wrong token",
            Some("Bearer not-the-token-0123456789-abcdefghij"),
        ),
        ("wrong last byte", Some(wrong_last.as_str())),
        ("lower-case scheme", Some(lower_scheme.as_str())),
        ("upper-case scheme", Some(upper_scheme.as_str())),
        ("two spaces", Some(two_spaces.as_str())),
        ("no scheme", Some(bare.as_str())),
        ("prefix of the token", Some(prefix.as_str())),
        ("longer than the token", Some(longer.as_str())),
        ("basic scheme", Some(basic.as_str())),
        ("empty", Some("")),
    ];
    for (case, authorization) in cases {
        assert_unauthorized(&post_with(&service, authorization, "<p>x</p>").await, case);
    }

    let correct = format!("Bearer {TOKEN}");
    let response = post_with(&service, Some(&correct), "<p>authorized</p>").await;
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(header(&response, "content-type"), "image/png");
    assert_common_headers(&response);

    // Health and routing stay as they were.
    let health = send(&service.authority, Method::GET, "/health", None, TIMEOUT)
        .await
        .unwrap();
    assert_json(&health, StatusCode::OK, r#"{"status":"ok"}"#);
    let unknown = send(&service.authority, Method::GET, "/render", None, TIMEOUT)
        .await
        .unwrap();
    assert_json(&unknown, StatusCode::NOT_FOUND, r#"{"error":"not found"}"#);

    // The token check comes before the busy check: without the token a client gets 401 while a
    // render runs, not 503, so it cannot learn whether the renderer is busy.
    let authority = service.authority.clone();
    let slow = tokio::spawn(async move {
        let body = Bytes::from(
            serde_json::to_vec(&serde_json::json!({"html": "<script>const t = Date.now(); while (Date.now() - t < 2000) {}</script>"}))
                .unwrap(),
        );
        send_with_authorization(
            &authority,
            Method::POST,
            "/render",
            Some(("application/json", body)),
            Some(&format!("Bearer {TOKEN}")),
            TIMEOUT,
        )
        .await
        .unwrap()
    });
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_unauthorized(&post_with(&service, None, "<p>x</p>").await, "while busy");
    assert_json(
        &post_with(&service, Some(&correct), "<p>x</p>").await,
        StatusCode::SERVICE_UNAVAILABLE,
        r#"{"error":"renderer busy"}"#,
    );
    assert_eq!(slow.await.unwrap().status, StatusCode::OK);
}

#[tokio::test]
async fn smoke_sends_the_token_from_the_same_settings() {
    let Some(chrome) = chrome() else { return };
    let dir = scratch_dir("token-file");
    let token_file = dir.join("render-token");
    std::fs::write(&token_file, format!("{TOKEN}\n")).unwrap();
    let token_file = token_file.to_str().unwrap();
    let binary = env!("CARGO_BIN_EXE_artifact-preview");
    for (label, service_env) in [
        ("value", ("RENDER_TOKEN", TOKEN)),
        ("file", ("RENDER_TOKEN_FILE", token_file)),
    ] {
        let service = Service::start(&chrome, &format!("smoke-{label}"), &[service_env]);
        let url = format!("http://{}", service.authority);
        let smoke = |env: &[(&str, &str)]| {
            let mut command = std::process::Command::new(binary);
            command
                .arg("smoke")
                .env_clear()
                .env("PREVIEW_RENDERER_URL", &url);
            for (name, value) in env {
                command.env(name, value);
            }
            command.output().unwrap()
        };
        for client_env in [("RENDER_TOKEN", TOKEN), ("RENDER_TOKEN_FILE", token_file)] {
            let passed = smoke(&[client_env]);
            assert!(
                passed.status.success(),
                "{label} service, {client_env:?}: {}",
                String::from_utf8_lossy(&passed.stderr)
            );
            assert!(
                String::from_utf8_lossy(&passed.stdout)
                    .starts_with("preview renderer smoke test passed (")
            );
        }
        let refused = smoke(&[]);
        assert!(
            !refused.status.success(),
            "{label}: smoke without the token"
        );
        let stderr = String::from_utf8_lossy(&refused.stderr);
        assert!(stderr.contains("renderer returned HTTP 401"), "{stderr}");
    }
    // A bad client setting stops `smoke` before it sends anything, without echoing the value.
    let output = std::process::Command::new(binary)
        .arg("smoke")
        .env_clear()
        .env("PREVIEW_RENDERER_URL", "http://127.0.0.1:1")
        .env("RENDER_TOKEN", "short-secret-value")
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("RENDER_TOKEN: the token must be"),
        "{stderr}"
    );
    assert!(!stderr.contains("short-secret-value"), "{stderr}");
    let _ = std::fs::remove_dir_all(dir);
}

/// Token mistakes stop the service at startup, and the message never contains a token.
#[test]
fn invalid_token_settings_stop_the_service_without_echoing_the_token() {
    let dir = scratch_dir("token-startup");
    let file = dir.join("render-token");
    std::fs::write(&file, format!("{TOKEN}\n")).unwrap();
    let doubled = dir.join("render-token-doubled");
    std::fs::write(&doubled, format!("{TOKEN}\n\n")).unwrap();
    let short = "s3cret-but-too-short";
    let file = file.to_str().unwrap();
    let doubled = doubled.to_str().unwrap();
    /// `(case, environment, expected message)`.
    type Case<'a> = (&'a str, &'a [(&'a str, &'a str)], &'a str);
    let cases: [Case; 4] = [
        (
            "both",
            &[("RENDER_TOKEN", TOKEN), ("RENDER_TOKEN_FILE", file)],
            "set RENDER_TOKEN or RENDER_TOKEN_FILE, not both",
        ),
        (
            "too short",
            &[("RENDER_TOKEN", short)],
            "RENDER_TOKEN: the token must be 32 to 4096 printable ASCII characters",
        ),
        (
            "two trailing newlines",
            &[("RENDER_TOKEN_FILE", doubled)],
            "RENDER_TOKEN_FILE: the token must be 32 to 4096 printable ASCII characters",
        ),
        (
            "missing file",
            &[("RENDER_TOKEN_FILE", "/nonexistent/render-token")],
            "RENDER_TOKEN_FILE (/nonexistent/render-token) cannot be read",
        ),
    ];
    for (case, env, message) in cases {
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_artifact-preview"));
        command
            .env_clear()
            .env("PORT", "1")
            .env("CHROME_HEADLESS_SHELL", "/bin/sh");
        for (name, value) in env {
            command.env(name, value);
        }
        let output = command.output().unwrap();
        assert!(!output.status.success(), "{case}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains(message), "{case}: {stderr}");
        assert!(!stderr.contains(TOKEN), "{case}: {stderr}");
        assert!(!stderr.contains(short), "{case}: {stderr}");
    }
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn refuses_to_start_without_a_browser() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_artifact-preview"))
        .env_clear()
        .env("PORT", "1")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("CHROME_HEADLESS_SHELL"));
}
