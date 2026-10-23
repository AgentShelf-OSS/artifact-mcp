// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
//! `reader-prefetch`: the next paragraphs are requested while the first plays, buffers follow
//! each other without a gap, one viewer runs at most two speech requests, and Stop aborts the
//! pending request, from `playwright/tests/reader-prefetch.cjs`.

use std::time::{Duration, Instant};

use browser_tests::Error;
use browser_tests::reader::{Audio, ReaderCtx};
use serde_json::{Value, json};

use crate::common::{errors, num, wait};

const INIT: &str = r#"(() => {
  window.__gap = { requests: [], starts: [], cancels: 0 };
  const NativeAudioContext = window.AudioContext || window.webkitAudioContext;
  if (NativeAudioContext) window.AudioContext = class extends NativeAudioContext {
    createBufferSource() {
      const source = super.createBufferSource(); const start = source.start.bind(source); const stop = source.stop.bind(source);
      const record = { start: null, wall: null, offset: 0, duration: 0, stopped: false }; window.__gap.starts.push(record);
      source.start = (...args) => { record.start = args[0]; record.wall = performance.now(); record.offset = args[1] || 0; record.rate = source.playbackRate.value; record.duration = source.buffer ? source.buffer.duration : 0; return start(...args); };
      source.stop = (...args) => { record.stopped = true; return stop(...args); }; return source;
    }
  };
  const nativeFetch = window.fetch.bind(window); window.fetch = async (input, init) => {
    const url = String(typeof input === 'string' ? input : input.url);
    let request;
    if (url.endsWith('/speech/stream')) {
      request = { at: performance.now(), index: window.__gap.requests.length };
      window.__gap.requests.push(request);
      init?.signal?.addEventListener('abort', () => { request.aborted = true; window.__gap.cancels++; }, { once: true });
    }
    try { return await nativeFetch(input, init); }
    catch (error) { if (request && error.name === 'AbortError') request.fetchAborted = true; throw error; }
  };
})()"#;

pub async fn run(t: ReaderCtx) -> Result<String, Error> {
    let fixture = t.fixture(Audio::seconds(2.0).latency(300)).await?;
    let html = "<!doctype html><html><body><main><h1>Gap test</h1><p>First paragraph deliberately lasts long enough to reveal network gaps.</p><p>Second paragraph should already be prefetched before the first ends.</p><p>Third paragraph verifies that read-ahead starts before the second paragraph plays.</p></main></body></html>";
    let body = fixture.publish("paragraph gap synthetic", html).await?;
    fixture.assert_speech_routes(&body.id).await?;
    // Keep the third request in flight so Stop must cancel a real pending HTTP request.
    fixture.worker().set_latency_for(|request| {
        request["text"]
            .as_str()
            .is_some_and(|text| text.starts_with("Third"))
            .then_some(10_000)
    });
    let page = t
        .page_with(Some((1200, 800)), &fixture.viewer_headers())
        .await?;
    page.add_init_script(INIT).await?;
    page.goto(&body.url).await?;
    crate::common::open_reader(&page).await?;
    page.locator("#vreader-voice")
        .select_option(&["qwen_reference"])
        .await?;
    // This spec measures prefetch seamlessness; authored pacing is covered by reader-pacing.
    page.locator("#vreader-pauses")
        .select_option(&["off"])
        .await?;
    page.locator("#vreader-play").click().await?;
    page.wait_for_function_with_timeout(
        "() => document.querySelector('#vreader-status')?.textContent?.includes('Reading')",
        &[],
        Duration::from_secs(5),
    )
    .await?;
    let prefetch_deadline = Instant::now() + Duration::from_secs(5);
    while fixture.worker().request_count() < 2 && Instant::now() < prefetch_deadline {
        wait(20).await;
    }
    assert!(
        fixture.worker().request_count() >= 2,
        "second request did not reach the worker"
    );
    // Let the first 2-second paragraph play through and allow the second transition to be
    // scheduled.
    wait(3800).await;
    let mut result = page
        .evaluate("({ gap: window.__gap, status: document.querySelector('#vreader-status')?.textContent, errors: [], now: performance.now() })")
        .await?;
    result["errors"] = json!(errors(&page));
    let report = serde_json::to_string_pretty(&result).unwrap_or_default();
    let requests = result["gap"]["requests"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(requests.len() >= 2, "expected second paragraph request");
    let second = &requests[1];
    let starts: Vec<Value> = result["gap"]["starts"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|item| item["stopped"] != json!(true) && !item["start"].is_null())
        .cloned()
        .collect();
    assert!(
        starts.len() >= 2,
        "expected two scheduled paragraph buffers\n{report}"
    );
    assert_eq!(
        num(&starts[0]["duration"]),
        2.0,
        "fixture must schedule one complete paragraph per buffer"
    );
    assert_eq!(
        num(&starts[1]["duration"]),
        2.0,
        "fixture must schedule one complete paragraph per buffer"
    );
    let (first_wall, first_duration, first_offset, first_rate) = (
        num(&starts[0]["wall"]),
        num(&starts[0]["duration"]),
        num(&starts[0]["offset"]),
        num(&starts[0]["rate"]),
    );
    let first_end_wall = first_wall + (first_duration - first_offset) * 1000.0 / first_rate;
    let second_at = num(&second["at"]);
    assert!(
        second_at < first_end_wall,
        "second request at {second_at:.1}ms was not prefetched before first playback ended at {first_end_wall:.1}ms"
    );
    // Read-ahead covers several blocks: the third paragraph is requested while the first
    // still plays.
    assert!(
        requests
            .get(2)
            .is_some_and(|third| num(&third["at"]) < first_end_wall),
        "third paragraph was not read ahead during the first"
    );
    let max_load = fixture.worker().max_load();
    assert!(
        max_load <= 2,
        "viewer ran {max_load} concurrent speech requests"
    );
    let transition_gap = (num(&starts[1]["start"]) - num(&starts[0]["start"]))
        - (first_duration - first_offset) / first_rate;
    assert!(
        transition_gap < 0.1,
        "paragraph transition gap {transition_gap}s exceeds 100ms"
    );
    page.locator("#vreader-stop").click().await?;
    page.wait_for_function_with_timeout(
        "() => window.__gap.requests.at(-1)?.fetchAborted === true",
        &[],
        Duration::from_secs(3),
    )
    .await?;
    assert_eq!(
        page.evaluate("window.__gap.requests.at(-1)?.aborted === true")
            .await?,
        json!(true),
        "stop did not abort active/prefetched stream"
    );
    let page_errors = errors(&page);
    assert_eq!(
        page_errors,
        Vec::<String>::new(),
        "{}",
        page_errors.join("; ")
    );
    Ok(report)
}
