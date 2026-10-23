// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
//! `CHROME_SANDBOX`, tested against the running binary: with the setting on, Chromium's own
//! sandbox is really active, which means that its renderer processes run under a seccomp-bpf
//! filter of their own. With the setting off, they do not.
//!
//! Each test samples `/proc/<pid>/status` of every renderer process below its own service while
//! a busy page keeps one render in flight.
//!
//! The sandbox needs unprivileged user namespaces and a user other than root. Without them the
//! sandbox-on test is skipped with a message, or fails when `CI` is set.

mod support;

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use artifact_preview::client::send;
use hyper::body::Bytes;
use hyper::{Method, StatusCode};
use support::{
    SeccompState, Service, chrome, chrome_sandbox_supported, process_tree, seccomp_state,
};

/// Keeps its renderer busy for 1.5 s while the document parses.
const BUSY_PAGE: &str =
    "<script>const until = Date.now() + 1500; while (Date.now() < until) {}</script><p>busy</p>";

/// The last seccomp state seen for each renderer process below `service` during one render of
/// [`BUSY_PAGE`], keyed by pid. A filter cannot be removed once attached, so the last sample of
/// a process is its most complete one.
async fn renderer_seccomp_during_render(service: &Service) -> BTreeMap<u32, SeccompState> {
    let authority = service.authority.clone();
    let render = tokio::spawn(async move {
        let body =
            Bytes::from(serde_json::to_vec(&serde_json::json!({"html": BUSY_PAGE})).unwrap());
        send(
            &authority,
            Method::POST,
            "/render",
            Some(("application/json", body)),
            Duration::from_secs(20),
        )
        .await
        .unwrap()
    });
    let mut samples = BTreeMap::new();
    let deadline = Instant::now() + Duration::from_secs(20);
    while !render.is_finished() && Instant::now() < deadline {
        for process in process_tree(service.pid()) {
            if process.cmdline.contains("--type=renderer")
                && let Some(state) = seccomp_state(process.pid)
            {
                samples.insert(process.pid, state);
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let response = render.await.unwrap();
    assert_eq!(
        response.status,
        StatusCode::OK,
        "the busy page must render: {}",
        String::from_utf8_lossy(&response.body)
    );
    assert!(
        !samples.is_empty(),
        "no renderer process was seen while the render was in flight"
    );
    samples
}

/// True if `state` has a seccomp filter that the service process (its ancestor) does not have.
/// The service's own state is the baseline, so a test runner that already runs under a seccomp
/// filter (a container, for example) does not count as Chromium's sandbox.
fn has_own_filter(state: SeccompState, baseline: SeccompState) -> bool {
    match (state.filters, baseline.filters) {
        (Some(filters), Some(inherited)) => state.mode == 2 && filters > inherited,
        // Before Linux 5.9 there is no filter count; only an unfiltered baseline is conclusive.
        _ => state.mode == 2 && baseline.mode == 0,
    }
}

#[tokio::test]
async fn sandbox_on_puts_renderers_under_seccomp() {
    let Some(chrome) = chrome() else { return };
    if !chrome_sandbox_supported() {
        return;
    }
    let service = Service::start(&chrome, "sandbox-on", &[("CHROME_SANDBOX", "1")]);
    let baseline = seccomp_state(service.pid()).expect("the service's seccomp state");
    let samples = renderer_seccomp_during_render(&service).await;
    eprintln!("CHROME_SANDBOX=1: service {baseline:?}, renderers {samples:?}");
    assert!(
        samples.values().any(|state| state.mode == 2),
        "no renderer process had `Seccomp: 2`: {samples:?}"
    );
    assert!(
        samples
            .values()
            .any(|&state| has_own_filter(state, baseline)),
        "no renderer process had a seccomp filter of its own (service {baseline:?}): {samples:?}"
    );
}

#[tokio::test]
async fn sandbox_off_leaves_renderers_without_a_filter_of_their_own() {
    let Some(chrome) = chrome() else { return };
    let service = Service::start(&chrome, "sandbox-off", &[("CHROME_SANDBOX", "0")]);
    let baseline = seccomp_state(service.pid()).expect("the service's seccomp state");
    let samples = renderer_seccomp_during_render(&service).await;
    eprintln!("CHROME_SANDBOX=0: service {baseline:?}, renderers {samples:?}");
    for (pid, state) in &samples {
        assert!(
            !has_own_filter(*state, baseline),
            "renderer {pid} has a seccomp filter of its own without CHROME_SANDBOX: {state:?}, \
             service {baseline:?}"
        );
        assert_eq!(
            (state.mode, state.filters),
            (baseline.mode, baseline.filters),
            "renderer {pid} differs from the service's seccomp state"
        );
    }
}

#[test]
fn an_invalid_chrome_sandbox_value_stops_the_service() {
    for value in ["yes", "TRUE", "2"] {
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_artifact-preview"))
            .env_clear()
            .env("PORT", "1")
            .env("CHROME_HEADLESS_SHELL", "/bin/sh")
            .env("CHROME_SANDBOX", value)
            .output()
            .unwrap();
        assert!(!output.status.success(), "{value}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("CHROME_SANDBOX must be 1, true, 0 or false"),
            "{value}: {stderr}"
        );
    }
}
