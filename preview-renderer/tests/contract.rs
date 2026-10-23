// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
//! The HTTP contract of the Node renderer, tested against the running `artifact-preview` binary.

mod support;

use std::io::{Read, Write};
use std::time::Duration;

use artifact_preview::client::{HttpResponse, send};
use artifact_preview::image_info::png_dimensions;
use hyper::body::Bytes;
use hyper::{Method, StatusCode};
use support::{Service, chrome};

const TIMEOUT: Duration = Duration::from_secs(20);

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
