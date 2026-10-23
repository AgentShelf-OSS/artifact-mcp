// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
//! `reader-player-scopes`: the real shell's view reading, mutation handling, detail reading,
//! scoped save and resume, and desktop and mobile layout, from
//! `playwright/tests/reader-player-scopes.cjs`.

use browser_tests::Error;
use browser_tests::reader::{Audio, ReaderCtx};
use serde_json::{Value, json};

use crate::common::{errors, wait};

const MESSAGES: &str =
    "(() => { window.messages = []; addEventListener('message', e => messages.push(e.data)); })()";

pub async fn run(t: ReaderCtx) -> Result<String, Error> {
    let fixture = t.fixture(Audio::seconds(15.0)).await?;
    let published = fixture
        .publish(
            "Mixed reader QA",
            r#"<main>
      <h1>Report</h1><p id="outside">Independent report text.</p>
      <section id="tool" data-artifact-reader-region="view"><h2>Current result</h2><p id="result">Forty tasks completed.</p><input value="private input"><button>Refresh</button></section>
      <pre id="code" style="padding:12px;background:#eee">echo ready</pre>
      <figure id="chart" data-artifact-reader-detail="Revenue rose every quarter, ending at forty units."><figcaption>Revenue trend</figcaption></figure>
    </main>"#,
        )
        .await?;
    fixture.assert_speech_routes(&published.id).await?;
    fixture.worker().set_duration(15.0);
    let p = t
        .page_with(Some((1280, 900)), &fixture.viewer_headers())
        .await?;
    p.add_init_script(MESSAGES).await?;
    p.goto(&published.url).await?;
    p.locator("#vreader-toggle").click().await?;
    p.locator("#vreader-expand").click().await?;
    p.locator("#vreader-voice")
        .select_option(&["bm_george"])
        .await?;
    // `p.frames().find(f => f.url().includes('/raw/'))`: the viewer's artifact frame.
    let frame = p.frame_locator("#vframe");
    assert!(
        p.frames().iter().any(|frame| frame.url.contains("/raw/")),
        "no raw artifact frame"
    );
    frame.locator("#result").click().await?;
    p.locator("#vreader-mode").select_option(&["view"]).await?;
    p.locator("#vreader-play").click().await?;
    p.wait_for_function(
        "() => messages.some(m => m.type === 'reader:content' && m.mode === 'view')",
        &[],
    )
    .await?;
    let view = p
        .evaluate("messages.find(m => m.type === 'reader:content' && m.mode === 'view')")
        .await?;
    assert_eq!(view["scopeKey"], json!("id:tool"));
    assert_eq!(
        crate::common::texts(&view["blocks"]),
        ["Current result", "Forty tasks completed."]
    );
    frame
        .locator("#outside")
        .evaluate("n => { n.textContent = 'Changed outside'; }", Value::Null)
        .await?;
    wait(300).await;
    assert_eq!(
        p.locator("#vreader-play").text_content().await?.as_deref(),
        Some("Pause")
    );
    frame
        .locator("#result")
        .evaluate(
            "n => { n.textContent = 'Forty-one tasks completed.'; }",
            Value::Null,
        )
        .await?;
    p.wait_for_function("() => messages.some(m => m.type === 'reader:changed')", &[])
        .await?;
    assert_eq!(
        p.locator("#vreader-play").text_content().await?.as_deref(),
        Some("Play")
    );
    frame.locator("#code").click().await?;
    p.locator("#vreader-target-details").click().await?;
    p.wait_for_function(
        "() => messages.some(m => m.type === 'reader:content' && m.mode === 'detail')",
        &[],
    )
    .await?;
    assert_eq!(
        p.evaluate(
            "messages.find(m => m.type === 'reader:content' && m.mode === 'detail').blocks[0].text"
        )
        .await?,
        json!("echo ready")
    );
    wait(500).await;
    p.locator("#vreader-play").click().await?;
    p.locator("#vreader-stop").click().await?;
    let detail_request = p
        .evaluate(
            "messages.find(m => m.type === 'reader:content' && m.mode === 'detail')?.requestId",
        )
        .await?;
    let checkpoints = p
        .evaluate("Object.entries(localStorage).filter(([k]) => k.startsWith('artifact-reader-place:')).map(([, v]) => JSON.parse(v))")
        .await?;
    assert!(
        checkpoints
            .as_array()
            .into_iter()
            .flatten()
            .any(|checkpoint| {
                checkpoint["version"] == json!(2)
                    && checkpoint["current"]["scopeKey"] == json!("id:code")
            }),
        "{checkpoints}"
    );
    p.locator("#vreader-resume").click().await?;
    p.wait_for_function(
        "(previous) => messages.some(m => m.type === 'reader:content' && m.mode === 'detail' && m.requestId !== previous)",
        &[detail_request],
    )
    .await?;
    p.locator("#vreader-stop").click().await?;
    // `selectOption({ label: 'Revenue trend' })`
    p.locator("#vreader-outline")
        .select_option(&["Revenue trend"])
        .await?;
    p.locator("#vreader-jump").focus().await?;
    p.keyboard().press("Enter").await?;
    p.wait_for_function(
        "() => messages.some(m => m.type === 'reader:content' && m.scopeKey === 'id:chart')",
        &[],
    )
    .await?;
    assert_eq!(
        p.evaluate("messages.find(m => m.type === 'reader:content' && m.scopeKey === 'id:chart').blocks[0].text")
            .await?,
        json!("Revenue rose every quarter, ending at forty units.")
    );
    p.locator("#vreader-stop").click().await?;
    assert_eq!(
        p.evaluate("document.documentElement.scrollWidth <= innerWidth")
            .await?,
        json!(true),
        "desktop reader overflow"
    );
    p.set_viewport(390, 844, 1.0).await?;
    assert_eq!(
        p.evaluate("document.documentElement.scrollWidth <= innerWidth")
            .await?,
        json!(true)
    );
    assert_eq!(errors(&p), Vec::<String>::new());
    Ok("PASS: real shell view reading, outside mutation ignored, inside mutation pauses, detail click, scoped save/resume, desktop/mobile, no page errors".into())
}
