// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
//! `reader-pacing`: Listen pacing, Media Session metadata and concurrent speech requests, from
//! `playwright/tests/reader-pacing.cjs`. Scheduled gaps are measured from
//! `AudioBufferSourceNode.start` times, so they are exact.

use browser_tests::reader::{Audio, DEFAULT_TITLE, ReaderCtx};
use browser_tests::{Error, Page};
use serde_json::{Value, json};

use crate::common::{num, wait};

const HTML: &str = "<main><h1>The Harbour</h1><p>First paragraph.</p><p>Second paragraph.</p><hr><p>After the break.</p></main>";

const INIT: &str = r#"(() => {
  window.__starts = [];
  const original = AudioBufferSourceNode.prototype.start;
  AudioBufferSourceNode.prototype.start = function (when = 0, ...rest) {
    if (this.buffer && this.buffer.duration > 0.02) window.__starts.push({ when, duration: this.buffer.duration / (this.playbackRate?.value || 1) });
    return original.call(this, when, ...rest);
  };
})()"#;

#[derive(Debug, Clone, Copy)]
struct Group {
    start: f64,
    end: f64,
}

/// Group buffer starts by gaps longer than 50 ms: each group is one spoken block. Waits up to
/// 200 × 50 ms for `count` groups.
async fn scheduled_paragraphs(page: &Page, count: usize) -> Result<Vec<Group>, Error> {
    for _ in 0..200 {
        let groups = page
            .evaluate(
                "(() => { const starts = (window.__starts || []).slice().sort((a, b) => a.when - b.when); const out = []; for (const item of starts) { const last = out[out.length - 1]; if (last && item.when - last.end < 0.05) last.end = Math.max(last.end, item.when + item.duration); else out.push({ start: item.when, end: item.when + item.duration }); } return out; })()",
            )
            .await?;
        let groups: Vec<Group> = groups
            .as_array()
            .into_iter()
            .flatten()
            .map(|group| Group {
                start: num(&group["start"]),
                end: num(&group["end"]),
            })
            .collect();
        if groups.len() >= count {
            return Ok(groups);
        }
        wait(50).await;
    }
    Err(Error::Assertion("not enough scheduled blocks".into()))
}

async fn stop_if_playing(page: &Page) -> Result<(), Error> {
    if page.locator("#vreader-stop").is_enabled().await? {
        page.locator("#vreader-stop").click().await?;
    }
    Ok(())
}

pub async fn run(t: ReaderCtx) -> Result<String, Error> {
    let fixture = t.fixture(Audio::seconds(1.0)).await?;
    let published = fixture.publish(DEFAULT_TITLE, HTML).await?;
    let page = t.page_with(None, &fixture.viewer_headers()).await?;
    page.add_init_script(INIT).await?;
    page.goto(&published.url).await?;
    page.locator("#vreader-toggle").click().await?;
    page.locator("#vreader-expand").click().await?;
    page.locator("#vreader-voice")
        .select_option(&["pocket_alba"])
        .await?;
    assert_eq!(
        page.locator("#vreader-pauses").input_value().await?,
        "natural"
    );

    // Natural pauses: about 0.9 s after a heading, 0.6 s between paragraphs, 1.8 s at a scene
    // break.
    page.locator("#vreader-play").click().await?;
    let groups = scheduled_paragraphs(&page, 4).await?;
    let gaps: Vec<f64> = (1..4)
        .map(|index| groups[index].start - groups[index - 1].end)
        .collect();
    let near = |value: f64, expected: f64| (value - expected).abs() < 0.25;
    assert!(near(gaps[0], 0.9), "after heading {:.3} s", gaps[0]);
    assert!(near(gaps[1], 0.6), "between paragraphs {:.3} s", gaps[1]);
    assert!(near(gaps[2], 1.8), "scene break {:.3} s", gaps[2]);

    // Lock-screen metadata is set while reading.
    let session = page
        .evaluate("(() => { const metadata = navigator.mediaSession && navigator.mediaSession.metadata; return metadata ? { title: metadata.title, artist: metadata.artist, album: metadata.album, artwork: metadata.artwork.map(item => item.src), state: navigator.mediaSession.playbackState } : null; })()")
        .await?;
    assert!(!session.is_null(), "media session metadata is set");
    assert_eq!(session["state"], json!("playing"));
    let filled = |name: &str| session[name].as_str().is_some_and(|text| !text.is_empty());
    assert!(
        filled("artist") && filled("album"),
        "artist and album are set"
    );
    assert!(
        session["artwork"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .any(|src| src.ends_with("/icons/app-192.png")),
        "artwork uses the app icon"
    );

    // A second speech request during playback is accepted (worker concurrency > 1).
    let second = page
        .call(
            "async (artifact) => (await fetch(`/${artifact}/speech`, { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ text: 'A preview line.', voice: 'pocket_alba' }) })).status",
            &[json!(published.id)],
        )
        .await?;
    assert_eq!(
        second,
        json!(200),
        "overlapping speech request is not rejected as busy"
    );
    stop_if_playing(&page).await?;

    // Pauses off: blocks follow each other without inserted silence.
    page.locator("#vreader-pauses")
        .select_option(&["off"])
        .await?;
    page.evaluate("window.__starts = []").await?;
    page.locator("#vreader-play").click().await?;
    let tight = scheduled_paragraphs(&page, 2).await.ok();
    let starts = page
        .evaluate("(window.__starts || []).slice().sort((a, b) => a.when - b.when)")
        .await?;
    let starts = starts.as_array().cloned().unwrap_or_default();
    let max_gap = starts
        .windows(2)
        .map(|pair| num(&pair[1]["when"]) - (num(&pair[0]["when"]) + num(&pair[0]["duration"])))
        .fold(f64::NEG_INFINITY, f64::max);
    assert!(
        tight.is_none() || max_gap < 0.15,
        "pauses off leaves {max_gap:.3} s"
    );
    stop_if_playing(&page).await?;
    let gaps: Vec<String> = gaps.iter().map(|gap| format!("{gap:.2}")).collect();
    Ok(format!(
        "PASS: pacing {} s, media session, concurrent request, pauses off",
        gaps.join("/")
    ))
}
