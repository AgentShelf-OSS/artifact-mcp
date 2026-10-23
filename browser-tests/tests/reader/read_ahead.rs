// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
//! `reader-read-ahead`: read-ahead fetches several blocks (at least 3, about 60 s, at most 8)
//! while one plays, and one viewer never has more than 2 speech requests at the worker at once,
//! from `playwright/tests/reader-read-ahead.cjs`.

use std::collections::HashSet;
use std::time::Duration;

use browser_tests::Error;
use browser_tests::reader::{Audio, ReaderCtx, now_millis};
use serde_json::json;

use crate::common::{errors, ordered, wait};

pub async fn run(t: ReaderCtx) -> Result<String, Error> {
    let fixture = t.fixture(Audio::seconds(4.0).latency(300)).await?;
    let paragraphs: String = (1..=14)
        .map(|index| {
            format!(
                "<p>Paragraph {index} reads one short sentence aloud for the read-ahead bound.</p>"
            )
        })
        .collect();
    let published = fixture
        .publish("read-ahead QA", &format!("<main>{paragraphs}</main>"))
        .await?;
    fixture.assert_speech_routes(&published.id).await?;
    let page = t
        .page_with(Some((1200, 800)), &fixture.viewer_headers())
        .await?;
    page.goto(&published.url).await?;
    crate::common::open_reader(&page).await?;
    page.locator("#vreader-voice")
        .select_option(&["qwen_reference"])
        .await?;
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
    let worker = fixture.worker();
    let first_at = worker
        .requests()
        .first()
        .map(|request| request.at)
        .ok_or_else(|| Error::Assertion("no speech request reached the worker".into()))?;
    // The first block lasts 4 s; read-ahead must fill its window well before that.
    let deadline = first_at + 3500;
    while worker.request_count() < 9 && now_millis() < deadline {
        wait(25).await;
    }
    let early = worker
        .requests()
        .iter()
        .filter(|item| item.at < first_at + 3500)
        .count();
    assert!(
        early >= 4,
        "only {} blocks were read ahead during the first block",
        early.saturating_sub(1)
    );
    assert!(
        early <= 9,
        "{} blocks read ahead; the bound is 8",
        early.saturating_sub(1)
    );
    wait(1500).await;
    let texts: Vec<String> = worker
        .requests()
        .iter()
        .map(|item| item.body["text"].as_str().unwrap_or("").to_owned())
        .collect();
    let unique: HashSet<&String> = texts.iter().collect();
    assert_eq!(unique.len(), texts.len(), "a block was synthesized twice");
    let max_load = worker.max_load();
    assert!(
        max_load <= 2,
        "viewer ran {max_load} concurrent speech requests"
    );
    page.locator("#vreader-stop").click().await?;
    assert_eq!(errors(&page), Vec::<String>::new());
    Ok(ordered(&[
        ("readAhead", json!(early - 1)),
        ("maxConcurrent", json!(max_load)),
        ("requests", json!(texts.len())),
    ]))
}
