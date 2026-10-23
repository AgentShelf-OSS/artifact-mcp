// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
//! Deadlines and cleanup of the running binary: hostile pages fail within the deadline, and no
//! browser process or temporary directory outlives a render, a disconnect, or the service.
//!
//! The tests scan for processes started from the browser executable, so they hold a lock and run
//! one at a time.

mod support;

use std::time::{Duration, Instant};

use artifact_preview::client::send;
use hyper::body::Bytes;
use hyper::{Method, StatusCode};
use support::{Service, chrome, chrome_processes};

const RENDER_TIMEOUT_MS: u64 = 3000;
/// Launch, kill and reap on a loaded machine: generous, but far below a second deadline.
const MARGIN: Duration = Duration::from_millis(1500);

static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn post_render(authority: &str, html: &str) -> (StatusCode, Bytes) {
    let body = Bytes::from(serde_json::to_vec(&serde_json::json!({"html": html})).unwrap());
    let response = send(
        authority,
        Method::POST,
        "/render",
        Some(("application/json", body)),
        Duration::from_secs(30),
    )
    .await
    .unwrap();
    (response.status, response.body)
}

fn assert_clean(service: &Service, context: &str) {
    let processes = service.browser_processes();
    assert!(
        processes.is_empty(),
        "{context}: browser processes left: {processes:#?}"
    );
    let dirs = service.render_dirs();
    assert!(
        dirs.is_empty(),
        "{context}: temporary directories left: {dirs:?}"
    );
}

#[tokio::test]
async fn hostile_pages_fail_at_the_deadline_and_leave_nothing() {
    let Some(chrome) = chrome() else { return };
    let _serial = SERIAL.lock().await;
    let timeout = RENDER_TIMEOUT_MS.to_string();
    let service = Service::start(&chrome, "deadline", &[("RENDER_TIMEOUT_MS", &timeout)]);
    let pages = [
        (
            "infinite loop while parsing",
            "<script>for (;;) {}</script>",
        ),
        (
            "endless blocked requests",
            "<script>for (;;) { try { const x = new XMLHttpRequest(); x.open('GET', 'http://127.0.0.1:9/', false); x.send(); } catch (e) {} }</script>",
        ),
        (
            "microtask loop after DOMContentLoaded",
            "<script>addEventListener('DOMContentLoaded', () => { const spin = () => Promise.resolve().then(spin); spin(); });</script>",
        ),
        (
            "infinite loop in onload",
            "<body onload='for (;;) {}'>x</body>",
        ),
        (
            "endless alert dialogs",
            "<script>for (;;) alert(1);</script>",
        ),
        (
            "endless popups",
            "<script>for (;;) window.open('about:blank');</script>",
        ),
        (
            "memory hog",
            "<script>const a = []; for (;;) a.push(new Array(1e6).fill(1));</script>",
        ),
    ];
    let deadline = Duration::from_millis(RENDER_TIMEOUT_MS);
    for (name, html) in pages {
        let started = Instant::now();
        let (status, body) = post_render(&service.authority, html).await;
        let elapsed = started.elapsed();
        eprintln!("{name}: HTTP {} after {elapsed:?}", status.as_u16());
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{name}");
        assert_eq!(&body[..], br#"{"error":"render failed"}"#, "{name}");
        // The memory hog may crash its renderer before the deadline; the others must hit it.
        if name != "memory hog" {
            assert!(elapsed >= deadline, "{name} failed early after {elapsed:?}");
        }
        assert!(elapsed <= deadline + MARGIN, "{name} took {elapsed:?}");
        assert_clean(&service, name);
    }
    // The service is still healthy and renders normally afterwards.
    let (status, _) = post_render(&service.authority, "<p>after</p>").await;
    assert_eq!(status, StatusCode::OK);
    assert_clean(&service, "after");
}

#[tokio::test]
async fn twenty_sequential_renders_leak_nothing() {
    let Some(chrome) = chrome() else { return };
    let _serial = SERIAL.lock().await;
    let service = Service::start(&chrome, "sequential", &[]);
    let fd_count = || {
        std::fs::read_dir(format!("/proc/{}/fd", service.pid()))
            .unwrap()
            .count()
    };
    let rss_kib = || {
        std::fs::read_to_string(format!("/proc/{}/status", service.pid()))
            .unwrap()
            .lines()
            .find_map(|line| {
                line.strip_prefix("VmRSS:")
                    .map(|rest| rest.trim().trim_end_matches(" kB").parse::<u64>().unwrap())
            })
            .unwrap()
    };
    // Warm up once so lazily created runtime state is not counted as a leak.
    assert_eq!(
        post_render(&service.authority, "<p>warm</p>").await.0,
        StatusCode::OK
    );
    let fds_before = fd_count();
    let rss_before = rss_kib();
    for index in 0..20 {
        let html =
            format!("<h1>render {index}</h1><p style='background:linear-gradient(red,blue)'>x</p>");
        let (status, _) = post_render(&service.authority, &html).await;
        assert_eq!(status, StatusCode::OK, "render {index}");
        assert_clean(&service, &format!("render {index}"));
    }
    let fds_after = fd_count();
    let rss_after = rss_kib();
    eprintln!("service fds {fds_before} -> {fds_after}, RSS {rss_before} KiB -> {rss_after} KiB");
    assert!(
        fds_after <= fds_before,
        "file descriptors leaked: {fds_before} -> {fds_after}"
    );
}

#[tokio::test]
async fn client_disconnect_mid_render_still_cleans_up() {
    let Some(chrome) = chrome() else { return };
    let _serial = SERIAL.lock().await;
    let service = Service::start(&chrome, "disconnect", &[]);
    let slow = "<script>const t = Date.now(); while (Date.now() - t < 1500) {}</script><p>slow</p>";
    let request = post_render(&service.authority, slow);
    // Give up on the request while the browser is busy.
    assert!(
        tokio::time::timeout(Duration::from_millis(600), request)
            .await
            .is_err()
    );
    let started = Instant::now();
    loop {
        let (status, _) = post_render(&service.authority, "<p>next</p>").await;
        if status == StatusCode::OK {
            break;
        }
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "render slot never freed"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_clean(&service, "after disconnect");
}

#[tokio::test]
async fn stopping_the_service_takes_the_browser_down() {
    let Some(chrome) = chrome() else { return };
    let _serial = SERIAL.lock().await;
    for signal in ["TERM", "KILL"] {
        let service = Service::start(&chrome, "stop", &[]);
        let authority = service.authority.clone();
        let slow = "<script>const t = Date.now(); while (Date.now() - t < 5000) {}</script>";
        let request = tokio::spawn(async move { post_render(&authority, slow).await });
        // Wait until the browser is up.
        let started = Instant::now();
        while chrome_processes(&chrome).is_empty() {
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "browser never started"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
        let status = std::process::Command::new("kill")
            .arg(format!("-{signal}"))
            .arg(service.pid().to_string())
            .status()
            .unwrap();
        assert!(status.success());
        let stopped = Instant::now();
        while !chrome_processes(&chrome).is_empty() {
            assert!(
                stopped.elapsed() < Duration::from_secs(3),
                "SIG{signal}: browser outlived the service: {:#?}",
                chrome_processes(&chrome)
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        eprintln!(
            "SIG{signal}: browser gone {:?} after the signal",
            stopped.elapsed()
        );
        request.abort();
        drop(service);
    }
}
