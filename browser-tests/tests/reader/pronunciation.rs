// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
//! `reader-pronunciation`: spoken replacements with the source DOM unchanged, highlight mapping
//! back to the written term, sentence replay, metadata invalidation and partial or whole-term
//! selections, from `playwright/tests/reader-pronunciation.cjs`.

use std::time::{Duration, Instant};

use browser_tests::reader::{Audio, ReaderCtx, Worker, WorkerRequest};
use browser_tests::{Error, FrameLocator};
use serde_json::{Value, json};

use crate::common::{errors, wait};

const MESSAGES: &str =
    "(() => { window.messages = []; addEventListener('message', e => messages.push(e.data)); })()";

/// `waitForSpeech(text, start)`: the first worker request from index `start` on whose text is
/// `text`, within 10 s.
async fn wait_for_speech(
    worker: &Worker,
    text: &str,
    start: usize,
) -> Result<WorkerRequest, Error> {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        let found = worker
            .requests()
            .into_iter()
            .skip(start)
            .find(|request| request.body["text"] == json!(text));
        if let Some(record) = found {
            return Ok(record);
        }
        wait(20).await;
    }
    Err(Error::Assertion(format!(
        "timed out waiting for speech request: {text}"
    )))
}

async fn word_is(frame: &FrameLocator<'_>, word: &str) -> Result<(), Error> {
    frame
        .wait_for_function(
            "(word) => { const h = CSS.highlights.get('artifact-reader-word'); return h && [...h][0].toString() === word; }",
            json!(word),
        )
        .await?;
    Ok(())
}

pub async fn run(t: ReaderCtx) -> Result<String, Error> {
    let fixture = t.fixture(Audio::seconds(12.0)).await?;
    let worker = fixture.worker();
    let published = fixture
        .publish(
            "Mixed reader QA",
            "<main><p id=\"passage\">Talk to <span id=\"name\" data-artifact-pronounce=\"Shiv awn\">Siobhan</span>. She reads <span data-artifact-pronounce=\"sequel\">SQL</span>.</p></main>",
        )
        .await?;
    fixture.assert_speech_routes(&published.id).await?;
    worker.set_duration(12.0);
    let p = t
        .page_with(Some((1280, 900)), &fixture.viewer_headers())
        .await?;
    p.add_init_script(MESSAGES).await?;
    p.goto(&published.url).await?;
    p.locator("#vreader-toggle").click().await?;
    p.locator("#vreader-expand").click().await?;
    p.locator("#vreader-voice")
        .select_option(&["pocket_alba"])
        .await?;
    let frame = p.frame_locator("#vframe");
    p.locator("#vreader-play").click().await?;
    p.wait_for_function(
        "() => document.querySelector('#vreader-status')?.textContent?.includes('Reading')",
        &[],
    )
    .await?;
    wait_for_speech(worker, "Talk to Shiv awn. She reads sequel.", 0).await?;
    word_is(&frame, "Siobhan").await?;
    assert_eq!(
        frame.locator("#passage").inner_text().await?,
        "Talk to Siobhan. She reads SQL."
    );
    assert_eq!(
        p.evaluate("artifactReaderDiagnostics()[0].firstScheduledAudioMs >= 0")
            .await?,
        json!(true)
    );
    word_is(&frame, "SQL").await?;
    p.locator("#vreader-play").click().await?;
    assert!(p.locator("#vreader-replay").is_enabled().await?);
    p.locator("#vreader-replay").click().await?;
    word_is(&frame, "She").await?;
    frame
        .locator("#name")
        .evaluate(
            "n => n.setAttribute('data-artifact-pronounce', 'Shiv on')",
            Value::Null,
        )
        .await?;
    p.wait_for_function("() => messages.some(m => m.type === 'reader:changed')", &[])
        .await?;
    frame
        .locator("#name")
        .evaluate(
            "n => { const r = document.createRange(); r.setStart(n.firstChild, 1); r.setEnd(n.firstChild, 4); getSelection().removeAllRanges(); getSelection().addRange(r); }",
            Value::Null,
        )
        .await?;
    wait(60).await;
    p.locator("#vreader-mode")
        .select_option(&["selection"])
        .await?;
    let partial_start = worker.request_count();
    p.locator("#vreader-play").click().await?;
    wait_for_speech(worker, "iob", partial_start).await?;
    p.locator("#vreader-stop").click().await?;
    frame
        .locator("#name")
        .evaluate(
            "n => { const r = document.createRange(); r.selectNodeContents(n); getSelection().removeAllRanges(); getSelection().addRange(r); }",
            Value::Null,
        )
        .await?;
    wait(60).await;
    p.locator("#vreader-mode")
        .select_option(&["selection"])
        .await?;
    let whole_start = worker.request_count();
    p.locator("#vreader-play").click().await?;
    wait_for_speech(worker, "Shiv on", whole_start).await?;
    word_is(&frame, "Siobhan").await?;
    p.locator("#vreader-stop").click().await?;
    assert_eq!(errors(&p), Vec::<String>::new());
    Ok("PASS: spoken replacements, source DOM unchanged, multiword-to-term highlighting, SQL mapping, sentence replay, metadata invalidation, partial and whole-term selection, bounded diagnostics".into())
}
