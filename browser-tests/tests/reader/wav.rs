// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
//! `reader-wav`: WAV playback through the `<audio>` element (the `bm_` voice): pause saves the
//! place, rewind 15 s, Play after a reload resumes there, and finishing clears the place, from
//! `playwright/tests/reader-wav.cjs`.

use std::time::Duration;

use browser_tests::reader::{Audio, ReaderCtx};
use browser_tests::{Error, Page, WaitState};
use serde_json::{Value, json};

use crate::common::{errors, num, ordered, wait};

const INIT: &str = r#"(() => {
  if (window.top !== window) return;
  localStorage.setItem('artifact-reader-preferences', JSON.stringify({ voice: 'bm_george', rate: '1' }));
  window.__wav = { speechCalls: 0 };
  const nativeFetch = window.fetch.bind(window);
  window.fetch = async (input, init) => {
    const url = String(typeof input === 'string' ? input : input.url);
    if (url.endsWith('/speech')) window.__wav.speechCalls++;
    return nativeFetch(input, init);
  };
})()"#;

const READING: &str = "() => document.querySelector('#vreader-audio')?.readyState >= 1 && document.querySelector('#vreader-status')?.textContent?.includes('Reading')";

async fn saved(page: &Page, key: &Value) -> Result<Value, Error> {
    page.call(
        "(key) => JSON.parse(localStorage.getItem(key))",
        std::slice::from_ref(key),
    )
    .await
}

async fn current_time(page: &Page) -> Result<f64, Error> {
    Ok(num(&page
        .evaluate("document.querySelector('#vreader-audio').currentTime")
        .await?))
}

pub async fn run(t: ReaderCtx) -> Result<String, Error> {
    let fixture = t.fixture(Audio::seconds(25.0).latency(30)).await?;
    let published = fixture
        .publish(
            "WAV resume QA",
            "<!doctype html><html><body><main><p>Twenty five seconds of synthetic narration.</p></main></body></html>",
        )
        .await?;
    fixture.assert_speech_routes(&published.id).await?;
    let page = t
        .page_with(Some((1200, 800)), &fixture.viewer_headers())
        .await?;
    page.add_init_script(INIT).await?;
    page.goto(&published.url).await?;
    crate::common::open_reader(&page).await?;
    page.locator("#vreader-play").click().await?;
    page.wait_for_function_with_timeout(READING, &[], Duration::from_secs(10))
        .await?;
    assert_eq!(
        page.evaluate("window.__wav.speechCalls >= 1").await?,
        json!(true),
        "first click did not request WAV speech"
    );
    page.evaluate("document.querySelector('#vreader-audio').currentTime = 18")
        .await?;
    page.wait_for_function_with_timeout(
        "() => { const audio = document.querySelector('#vreader-audio'); return !audio.seeking && audio.currentTime >= 17.5; }",
        &[],
        Duration::from_secs(3),
    )
    .await?;
    page.locator("#vreader-play").click().await?;
    page.wait_for_function_with_timeout(
        "() => Object.keys(localStorage).some(key => { if (!key.startsWith('artifact-reader-place:')) return false; try { return Math.abs(JSON.parse(localStorage.getItem(key)).current.offset - 18) < 0.4; } catch { return false; } })",
        &[],
        Duration::from_secs(3),
    )
    .await?;
    let key = page
        .evaluate("Object.keys(localStorage).find(key => key.startsWith('artifact-reader-place:'))")
        .await?;
    let saved18 = saved(&page, &key).await?;
    let offset18 = num(&saved18["current"]["offset"]);
    assert!(
        (offset18 - 18.0).abs() < 0.4,
        "pause checkpoint offset was {offset18}"
    );
    page.locator("#vreader-rewind").click().await?;
    wait(100).await;
    let rewound = current_time(&page).await?;
    let saved3 = saved(&page, &key).await?;
    let offset3 = num(&saved3["current"]["offset"]);
    assert!(
        (rewound - 3.0).abs() < 0.4,
        "rewind currentTime was {rewound}"
    );
    assert!(
        (offset3 - 3.0).abs() < 0.4,
        "rewind checkpoint offset was {offset3}"
    );
    page.reload().await?;
    crate::common::open_reader(&page).await?;
    page.locator("#vreader-resume")
        .wait_for(WaitState::Visible)
        .await?;
    // Play after a reload continues from the saved place, as the separate Resume button does.
    page.locator("#vreader-play").click().await?;
    page.wait_for_function_with_timeout(READING, &[], Duration::from_secs(10))
        .await?;
    let resumed = current_time(&page).await?;
    assert!(
        (resumed - 3.0).abs() < 0.6,
        "reload resume currentTime was {resumed}"
    );
    page.locator("#vreader-play").click().await?;
    page.wait_for_function_with_timeout(
        "() => document.querySelector('#vreader-audio').paused && document.querySelector('#vreader-status')?.textContent.startsWith('Paused')",
        &[],
        Duration::from_secs(3),
    )
    .await?;
    page.evaluate("document.querySelector('#vreader-audio').currentTime = 24.95")
        .await?;
    page.locator("#vreader-play").click().await?;
    page.wait_for_function_with_timeout(
        "() => document.querySelector('#vreader-status')?.textContent === 'Finished'",
        &[],
        Duration::from_secs(10),
    )
    .await?;
    assert_eq!(
        page.call(
            "(key) => localStorage.getItem(key)",
            std::slice::from_ref(&key),
        )
        .await?,
        Value::Null,
        "finished playback left a checkpoint"
    );
    let page_errors = errors(&page);
    assert_eq!(
        page_errors,
        Vec::<String>::new(),
        "{}",
        page_errors.join("; ")
    );
    Ok(ordered(&[
        (
            "speechCalls",
            page.evaluate("window.__wav.speechCalls").await?,
        ),
        ("saved18", saved18["current"]["offset"].clone()),
        ("rewound", json!(rewound)),
        ("saved3", saved3["current"]["offset"].clone()),
        ("resumed", json!(resumed)),
        ("finished", json!(true)),
        ("errors", json!(page_errors)),
    ]))
}
