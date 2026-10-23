// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
//! `reader-forward`: Forward 30 s seeks inside the current block when its audio is there,
//! otherwise it carries the remainder into the next block (the mirror of rewind 15 s), from
//! `playwright/tests/reader-forward.cjs`.

use std::time::Duration;

use browser_tests::Error;
use browser_tests::reader::{Audio, ReaderCtx};
use serde_json::json;

use crate::common::{checkpoint, errors, num, ordered, preferences_script, wait};

/// About 480 characters: the 15 characters-a-second estimate gives each block about 32 s.
fn paragraph(word: &str) -> String {
    (1..=6)
        .map(|index| {
            format!("{word} sentence {index} carries enough words to fill a long paragraph for skipping.")
        })
        .collect::<Vec<_>>()
        .join(" ")
}

pub async fn run(t: ReaderCtx) -> Result<String, Error> {
    let fixture = t.fixture(Audio::seconds(40.0).latency(30)).await?;
    let html = format!(
        "<main><p>{}</p><p>{}</p><p>{}</p></main>",
        paragraph("Alpha"),
        paragraph("Bravo"),
        paragraph("Charlie")
    );
    let published = fixture.publish("forward QA", &html).await?;
    fixture.assert_speech_routes(&published.id).await?;
    let page = t
        .page_with(Some((1200, 800)), &fixture.viewer_headers())
        .await?;
    page.add_init_script(&preferences_script("pocket_alba", "1"))
        .await?;
    page.goto(&published.url).await?;
    crate::common::open_reader(&page).await?;
    let forward = page.locator("#vreader-forward");
    assert_eq!(
        forward.get_attribute("aria-label").await?.as_deref(),
        Some("Forward 30 seconds")
    );
    assert!(
        !forward.is_enabled().await?,
        "forward enabled before audio loaded"
    );
    let current = || async { Ok::<_, Error>(checkpoint(&page).await?["current"].clone()) };
    page.locator("#vreader-play").click().await?;
    page.wait_for_function_with_timeout(
        "() => document.querySelector('.vreader').dataset.streamState === 'playing'",
        &[],
        Duration::from_secs(10),
    )
    .await?;
    wait(1500).await;
    page.locator("#vreader-play").click().await?;
    let start = current().await?;
    assert_eq!(num(&start["ordinal"]), 0.0);

    forward.click().await?;
    let within = current().await?;
    assert_eq!(
        num(&within["ordinal"]),
        0.0,
        "forward left a block that still had 30 s of audio"
    );
    let moved = num(&within["offset"]) - num(&start["offset"]);
    assert!((moved - 30.0).abs() < 0.05, "forward moved {moved} s");
    assert_eq!(
        page.locator("#vreader-play")
            .text_content()
            .await?
            .as_deref(),
        Some("Resume"),
        "forward changed the paused state"
    );

    forward.click().await?;
    page.wait_for_function_with_timeout(
        "() => { const k = Object.keys(localStorage).find(k => k.startsWith('artifact-reader-place:')); return k && JSON.parse(localStorage.getItem(k)).current.ordinal === 1; }",
        &[],
        Duration::from_secs(8),
    )
    .await?;
    let next = current().await?;
    let expected = num(&start["offset"]) + 60.0 - 40.0;
    assert!(
        (num(&next["offset"]) - expected).abs() < 0.1,
        "remainder offset {}, expected {}",
        num(&next["offset"]),
        num(&start["offset"]) + 20.0
    );
    assert_eq!(
        page.locator("#vreader-play")
            .text_content()
            .await?
            .as_deref(),
        Some("Resume"),
        "forward across blocks started playback"
    );
    let page_errors = errors(&page);
    assert_eq!(page_errors, Vec::<String>::new());
    Ok(ordered(&[
        ("forwardWithinBlock", json!(true)),
        ("forwardRemainderIntoNextBlock", json!(true)),
        ("errors", json!(page_errors)),
    ]))
}
