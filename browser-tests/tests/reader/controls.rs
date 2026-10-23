// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
//! `reader-controls`: section scope, end-of-section sleep without prefetch, wall-clock sleep
//! (also for a backgrounded tab), full-chunk resume, stream rewind by 15 s and preview
//! checkpoint isolation, from `playwright/tests/reader-controls.cjs`.

use std::time::Duration;

use browser_tests::reader::{Audio, ReaderCtx};
use browser_tests::{Error, Page};
use serde_json::{Value, json};

use crate::common::{errors, num, ordered, status, wait};

const INIT: &str = r#"(() => {
  if (window.top !== window) return;
  localStorage.setItem('artifact-reader-preferences', JSON.stringify({ voice: 'pocket_alba', rate: '1.25' }));
  localStorage.setItem('artifact-reader-style', JSON.stringify({ style: 'custom', custom: 'stale Qwen instruction' }));
  window.__pocket = { requests: [], cancels: 0 }; window.__clockOffset = 0;
  const realNow = Date.now; Date.now = () => realNow() + window.__clockOffset;
  const nativeFetch = window.fetch.bind(window);
  window.fetch = async (input, init) => {
    const url = String(typeof input === 'string' ? input : input.url);
    if (/\/speech\/stream(?:-timed)?$/.test(url)) window.__pocket.requests.push({ body: JSON.parse(init.body), at: performance.now() });
    try { return await nativeFetch(input, init); } catch (error) { if (error.name === 'AbortError') window.__pocket.cancels++; throw error; }
  };
})()"#;

/// The raw saved place (`localStorage.getItem(...)`), or `null`.
async fn checkpoint(page: &Page) -> Result<Value, Error> {
    page.evaluate("(() => { const k = Object.keys(localStorage).find(k => k.startsWith('artifact-reader-place:')); return k ? localStorage.getItem(k) : null; })()")
        .await
}

fn parse(raw: &Value) -> Value {
    raw.as_str()
        .and_then(|text| serde_json::from_str(text).ok())
        .unwrap_or(Value::Null)
}

async fn finished(page: &Page) -> Result<(), Error> {
    page.wait_for_function_with_timeout(
        "() => /Finished|Sleep timer ended/.test(document.querySelector('#vreader-status').textContent)",
        &[],
        Duration::from_secs(10),
    )
    .await?;
    Ok(())
}

async fn requested_texts(page: &Page) -> Result<Value, Error> {
    page.evaluate("__pocket.requests.map(r => r.body.text)")
        .await
}

pub async fn run(t: ReaderCtx) -> Result<String, Error> {
    let fixture = t.fixture(Audio::seconds(0.5).latency(30)).await?;
    let html = "<main><section><h1>First section</h1><p id=first>First paragraph.</p></section><section><h2>Second section</h2><p id=second>Second paragraph.</p></section></main>";
    let published = fixture.publish("pocket QA", html).await?;
    fixture.assert_speech_routes(&published.id).await?;
    let page = t
        .page_with(Some((1200, 800)), &fixture.viewer_headers())
        .await?;
    page.add_init_script(INIT).await?;
    page.goto(&published.url).await?;
    crate::common::open_reader(&page).await?;
    let iframe = page.frame_locator("#vframe");
    iframe
        .locator("#first")
        .dispatch_event("pointerdown")
        .await?;
    page.locator("#vreader-mode")
        .select_option(&["section"])
        .await?;
    page.locator("#vreader-play").click().await?;
    finished(&page).await?;
    assert_eq!(
        requested_texts(&page).await?,
        json!(["First section", "First paragraph."]),
        "section mode crossed sibling"
    );
    assert_eq!(
        page.locator("#vreader-sleep option[value=chapter]")
            .evaluate("el => el.disabled && el.hidden", Value::Null)
            .await?,
        json!(true),
        "generic chapter timer enabled"
    );
    page.evaluate("__pocket.requests = []").await?;
    page.locator("#vreader-mode")
        .select_option(&["page"])
        .await?;
    page.locator("#vreader-sleep")
        .select_option(&["section"])
        .await?;
    page.locator("#vreader-play").click().await?;
    finished(&page).await?;
    let text = status(&page).await?;
    assert!(text.contains("Sleep timer ended"), "{text}");
    assert_eq!(
        requested_texts(&page).await?,
        json!(["First section", "First paragraph."]),
        "sleep endsection prefetched sibling"
    );
    assert!(
        crate::common::truthy(&checkpoint(&page).await?),
        "boundary sleep did not save place"
    );
    // A timed sleep must stop ongoing audio, including when its tab was backgrounded.
    fixture.worker().set_duration(25.0);
    page.evaluate("__pocket.requests = []").await?;
    page.locator("#vreader-sleep")
        .select_option(&["15"])
        .await?;
    page.locator("#vreader-play").click().await?;
    page.wait_for_function(
        "() => document.querySelector('.vreader').dataset.streamState === 'playing'",
        &[],
    )
    .await?;
    wait(14_000).await;
    page.locator("#vreader-play").click().await?;
    let before_rewind = parse(&checkpoint(&page).await?);
    let request_count = page.evaluate("__pocket.requests.length").await?;
    if num(&before_rewind["current"]["offset"]) <= 15.0 {
        let state = page
            .locator(".vreader")
            .get_attribute("data-stream-state")
            .await?;
        let worker: Vec<Value> = fixture
            .worker()
            .requests()
            .into_iter()
            .map(|request| json!({"path": request.path, "body": request.body, "at": request.at as f64}))
            .collect();
        panic!(
            "stream did not reach 15 seconds: {}",
            json!({"checkpoint": before_rewind, "requests": request_count, "state": state, "status": status(&page).await?, "worker": worker})
        );
    }
    page.locator("#vreader-rewind").click().await?;
    let after_rewind = parse(&checkpoint(&page).await?);
    let (before, after) = (
        num(&before_rewind["current"]["offset"]),
        num(&after_rewind["current"]["offset"]),
    );
    assert!(
        (before - after - 15.0).abs() < 0.05,
        "stream rewind did not move 15 media seconds: {before} -> {after}"
    );
    assert_eq!(
        page.evaluate("__pocket.requests.length").await?,
        request_count,
        "current stream rewind refetched audio"
    );
    assert_eq!(
        page.locator("#vreader-play")
            .text_content()
            .await?
            .as_deref(),
        Some("Resume"),
        "rewind changed paused state"
    );

    page.evaluate(
        "window.__clockOffset = 16 * 60000; document.dispatchEvent(new Event('visibilitychange'));",
    )
    .await?;
    let text = status(&page).await?;
    assert!(text.contains("Sleep timer ended"), "{text}");
    assert_eq!(
        page.locator("#vreader-play")
            .text_content()
            .await?
            .as_deref(),
        Some("Play")
    );
    assert_eq!(page.locator("#vreader-sleep").input_value().await?, "off");
    assert!(num(&parse(&checkpoint(&page).await?)["current"]["offset"]) > 0.0);
    let before_preview = checkpoint(&page).await?;
    fixture.worker().set_duration(0.5);
    page.evaluate("__clockOffset = 0").await?;
    page.locator("#vreader-preview").click().await?;
    wait(100).await;
    assert_eq!(
        checkpoint(&page).await?,
        before_preview,
        "preview changed existing checkpoint during playback"
    );
    page.wait_for_function_with_timeout(
        "() => document.querySelector('#vreader-status').textContent.includes('Preview finished')",
        &[],
        Duration::from_secs(5),
    )
    .await?;
    assert_eq!(
        checkpoint(&page).await?,
        before_preview,
        "preview changed existing checkpoint after completion"
    );
    // Resume skips a fully completed chunk and must progress, not leave loading stuck.
    fixture.worker().set_duration(0.5);
    page.evaluate("(() => { window.__clockOffset = 0; const k = Object.keys(localStorage).find(k => k.startsWith('artifact-reader-place:')); const v = JSON.parse(localStorage.getItem(k)); v.current.offset = .5; localStorage.setItem(k, JSON.stringify(v)); })()")
        .await?;
    page.locator("#vreader-resume").click().await?;
    finished(&page).await?;
    let text = status(&page).await?;
    assert!(text.contains("Finished"), "{text}");
    let page_errors = errors(&page);
    assert_eq!(page_errors, Vec::<String>::new());
    Ok(ordered(&[
        ("sectionScope", json!(true)),
        ("endSectionNoPrefetch", json!(true)),
        ("wallClockSleep", json!(true)),
        ("fullChunkResume", json!(true)),
        ("streamRewind15", json!(true)),
        ("previewCheckpointIsolation", json!(true)),
        ("errors", json!(page_errors)),
    ]))
}
