// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
//! Browser-backed tests that need crate internals: each network defence layer on its own, and the
//! process-group boundary. The HTTP contract is tested against the binary in `tests/`.

#[path = "../tests/support/common.rs"]
mod common;
#[path = "../tests/support/vectors.rs"]
mod vectors;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Semaphore;

use crate::browser::Hardening;
use crate::render::Renderer;
use common::{Canary, canary_hosts, chrome, process_tree, running_processes};

/// Time each vector page stays loaded before the screenshot.
const SETTLE: Duration = Duration::from_millis(1000);
const PARALLEL_RENDERS: usize = 4;

/// Render every vector against every canary host under `hardening`. Each (vector, host) pair gets
/// its own canary, so hits are attributed exactly. Returns `name@host → (tcp hits, udp hits)`.
async fn probe(chrome: PathBuf, hardening: Hardening) -> BTreeMap<String, (usize, usize)> {
    let renderer = Renderer::new(chrome, Duration::from_secs(15))
        .with_hardening(hardening)
        .with_settle(SETTLE);
    let limit = Arc::new(Semaphore::new(PARALLEL_RENDERS));
    let mut tasks = Vec::new();
    for host in canary_hosts() {
        let names: Vec<&'static str> = vectors::vectors("h", 1, 2)
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        for name in names {
            let renderer = renderer.clone();
            let limit = Arc::clone(&limit);
            let host = host.to_string();
            tasks.push(tokio::spawn(async move {
                let _permit = limit.acquire_owned().await.unwrap();
                let canary = Canary::start();
                let page = vectors::vectors(&host, canary.tcp_port, canary.udp_port)
                    .into_iter()
                    .find(|(vector, _)| *vector == name)
                    .map(|(_, page)| page)
                    .unwrap();
                let _ = renderer.render(&page, 400, 300).await;
                // Late packets get a moment to arrive before counting.
                tokio::time::sleep(Duration::from_millis(200)).await;
                let hits = canary.hits();
                if hits != (0, 0) {
                    eprintln!("{name}@{host}: {hits:?} {:?}", canary.log.lock().unwrap());
                }
                (format!("{name}@{host}"), hits)
            }));
        }
    }
    let mut results = BTreeMap::new();
    for task in tasks {
        let (key, hits) = task.await.unwrap();
        results.insert(key, hits);
    }
    results
}

fn leaks(results: &BTreeMap<String, (usize, usize)>) -> Vec<String> {
    results
        .iter()
        .filter(|(_, hits)| **hits != (0, 0))
        .map(|(key, hits)| format!("{key} {hits:?}"))
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn full_hardening_blocks_every_vector() {
    let Some(chrome) = chrome() else { return };
    let results = probe(chrome, Hardening::FULL).await;
    assert!(!results.is_empty());
    assert_eq!(leaks(&results), Vec::<String>::new());
}

/// Interception plus offline emulation, without the launch flags: the Node renderer's model.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn interception_layer_alone() {
    let Some(chrome) = chrome() else { return };
    let results = probe(
        chrome,
        Hardening {
            intercept_requests: true,
            offline: true,
            network_flags: false,
        },
    )
    .await;
    let leaked = leaks(&results);
    eprintln!("interception + offline only, leaks: {leaked:#?}");
    // Request interception never sees WebSocket, WebRTC or WebTransport traffic, nor the
    // browser's own speculation-rules prefetch and prerender loads; the launch flags stop those.
    // Anything else getting through would be a regression in this layer.
    let known_gaps = ["websocket@", "webrtc-", "webtransport-", "speculation-"];
    let unexpected: Vec<&String> = leaked
        .iter()
        .filter(|entry| !known_gaps.iter().any(|kind| entry.starts_with(kind)))
        .collect();
    assert!(
        unexpected.is_empty(),
        "interception let through: {unexpected:?}"
    );
}

/// The launch flags alone, without interception or offline emulation.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn network_flags_layer_alone() {
    let Some(chrome) = chrome() else { return };
    let results = probe(
        chrome,
        Hardening {
            intercept_requests: false,
            offline: false,
            network_flags: true,
        },
    )
    .await;
    assert_eq!(leaks(&results), Vec::<String>::new());
}

/// With every defence off, the vectors must actually reach the canary; otherwise the tests above
/// would pass without proving anything.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn vectors_reach_the_canary_without_defences() {
    let Some(chrome) = chrome() else { return };
    let results = probe(
        chrome,
        Hardening {
            intercept_requests: false,
            offline: false,
            network_flags: false,
        },
    )
    .await;
    let reached: Vec<&String> = results
        .iter()
        .filter(|(_, hits)| **hits != (0, 0))
        .map(|(key, _)| key)
        .collect();
    let silent: Vec<&String> = results
        .iter()
        .filter(|(_, hits)| **hits == (0, 0))
        .map(|(key, _)| key)
        .collect();
    eprintln!(
        "control: {} of {} reached the canary; silent: {silent:#?}",
        reached.len(),
        results.len()
    );
    for required in [
        "img@127.0.0.1",
        "fetch@127.0.0.1",
        "websocket@127.0.0.1",
        "iframe@127.0.0.1",
        "form-submit@127.0.0.1",
    ] {
        assert!(
            reached.iter().any(|key| *key == required),
            "control vector {required} did not reach the canary"
        );
    }
}

#[tokio::test]
async fn every_browser_process_is_in_the_group_and_none_survives() {
    let Some(chrome) = chrome() else { return };
    let renderer = Renderer::new(chrome, Duration::from_secs(15));
    let page = format!(
        "<p>group</p><iframe srcdoc='<p>child</p>'></iframe><img src='data:image/png;base64,{}'>",
        common::RED_PNG_BASE64
    );
    let mut tree = Vec::new();
    let mut group = Vec::new();
    let mut leader = 0;
    let png = renderer
        .render_inspecting(&page, 400, 300, |browser| {
            leader = browser.pgid();
            tree = process_tree(leader);
            group = running_processes()
                .into_iter()
                .filter(|process| process.pgrp == leader)
                .collect();
        })
        .await
        .unwrap();
    assert!(png.starts_with(&crate::image_info::PNG_SIGNATURE));
    assert!(
        tree.len() >= 3,
        "expected browser, zygote and renderer processes, saw {tree:#?}"
    );
    for process in &tree {
        assert_eq!(
            process.pgrp, leader,
            "process {} ({}) left the browser's process group",
            process.pid, process.cmdline
        );
    }
    let mut tree_pids: Vec<u32> = tree.iter().map(|process| process.pid).collect();
    let mut group_pids: Vec<u32> = group.iter().map(|process| process.pid).collect();
    tree_pids.sort_unstable();
    group_pids.sort_unstable();
    assert_eq!(
        tree_pids, group_pids,
        "the process group and the process tree differ"
    );

    let survivors: Vec<_> = running_processes()
        .into_iter()
        .filter(|process| {
            process.pgrp == leader
                || tree
                    .iter()
                    .any(|seen| seen.pid == process.pid && seen.start_time == process.start_time)
        })
        .collect();
    assert!(
        survivors.is_empty(),
        "processes survived the render: {survivors:#?}"
    );
}
