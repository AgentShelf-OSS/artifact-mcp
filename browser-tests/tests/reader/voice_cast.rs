// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
//! `reader-voice-cast`: cast roles end to end (role controls, per-segment speech voices, inline
//! dialogue splits, ereader spoken headings and intros, and the untagged fallback), from
//! `playwright/tests/reader-voice-cast.cjs`.

use browser_tests::reader::{Audio, DEFAULT_TITLE, ReaderCtx, Worker, WorkerRequest};
use browser_tests::{Error, FrameLocator, Page, WaitState};
use serde_json::{Value, json};

use crate::common::wait;

fn cast() -> String {
    let cast = json!({
        "version": 1,
        "default": "narrator",
        "roles": [
            {"id": "narrator", "label": "Narrator"},
            {"id": "victor", "label": "Victor", "description": "Writes letters.", "suggest": ["pocket_alba"]},
            {"id": "creature", "label": "The creature", "description": "Speaks in dialogue."},
        ],
    });
    format!("<script type=\"application/json\" id=\"artifact-reader-cast\">{cast}</script>")
}

/// A minimal ereader in the shape the ereader adapter recognises.
fn ereader() -> String {
    let book = json!({"chapters": [{"part": "Part One", "title": "Chapter 1", "blocks": [
        {"t": "p", "s": "I walked out into the snow."},
        {"t": "p", "s": "He said, \"Wait for me,\" and vanished."},
    ]}]});
    format!(
        r#"{}<script id="book" type="application/json">{book}</script>
<article id="article"><header class="ch-head" data-artifact-reader-voice="narrator" data-artifact-reader-summary="Chapter One. Part One.">
<div class="part">Part One</div><h1>Chapter 1</h1><div class="meta">12 words</div>
<p class="recap" data-artifact-reader-intro>Previously, nothing happened.</p></header>
<p data-p="0" data-artifact-reader-voice="victor">I walked out into the snow.</p>
<p data-p="1" data-artifact-reader-voice="victor">He said, <span data-artifact-reader-voice="creature">"Wait for me,"</span> and vanished.</p>
</article>"#,
        cast()
    )
}

/// The worker requests from index `before` on, once every text in `texts` is among them (or
/// after 160 × 50 ms, whatever arrived).
async fn requests_for(worker: &Worker, before: usize, texts: &[&str]) -> Vec<WorkerRequest> {
    for _ in 0..160 {
        let seen: Vec<WorkerRequest> = worker.requests().into_iter().skip(before).collect();
        if texts
            .iter()
            .all(|text| seen.iter().any(|item| item.body["text"] == json!(text)))
        {
            return seen;
        }
        wait(50).await;
    }
    worker.requests().into_iter().skip(before).collect()
}

fn voice_of(seen: &[WorkerRequest], text: &str) -> Value {
    seen.iter()
        .find(|item| item.body["text"] == json!(text))
        .map(|item| item.body["voice"].clone())
        .unwrap_or(Value::Null)
}

async fn open(page: &Page) -> Result<(), Error> {
    page.locator("#vreader-toggle").click().await?;
    page.locator("#vreader-expand").click().await
}

async fn send(page: &Page, message: Value) -> Result<(), Error> {
    page.call(
        "(value) => document.querySelector('iframe').contentWindow.postMessage(value, '*')",
        &[message],
    )
    .await?;
    Ok(())
}

async fn marked(frame: &FrameLocator<'_>, name: &str) -> Result<Value, Error> {
    frame
        .evaluate(
            "(value) => { const found = CSS.highlights.get(value); return found ? [...found].map(range => range.toString()).join('') : null; }",
            json!(name),
        )
        .await
}

pub async fn run(t: ReaderCtx) -> Result<String, Error> {
    let fixture = t.fixture(Audio::seconds(1.0)).await?;
    let worker = fixture.worker();
    let generic = fixture
        .publish(
            DEFAULT_TITLE,
            &format!(
                "{}<main><p>Before the letter.</p><p data-artifact-reader-voice=\"victor\">Dear Mara.</p></main>",
                cast()
            ),
        )
        .await?;
    let book = fixture.publish(DEFAULT_TITLE, &ereader()).await?;
    let plain = fixture
        .publish(DEFAULT_TITLE, "<main><p>One ordinary paragraph.</p></main>")
        .await?;
    let page = t.page_with(None, &fixture.viewer_headers()).await?;
    let roles = page.locator("#vreader-cast-roles select");

    // Generic artifact: roles render, a role voice applies to its tagged block.
    page.goto(&generic.url).await?;
    open(&page).await?;
    page.locator("#vreader-cast")
        .wait_for(WaitState::Visible)
        .await?;
    assert_eq!(
        page.locator("#vreader-cast-roles .vreader-cast-role")
            .count()
            .await?,
        3
    );
    page.locator("#vreader-voice")
        .select_option(&["bm_george"])
        .await?;
    roles.nth(1).select_option(&["pocket_alba"]).await?;
    let before = worker.request_count();
    page.locator("#vreader-play").click().await?;
    let seen = requests_for(worker, before, &["Before the letter.", "Dear Mara."]).await;
    assert_eq!(
        voice_of(&seen, "Dear Mara."),
        json!("pocket_alba"),
        "tagged block uses its role voice"
    );
    assert_eq!(
        voice_of(&seen, "Before the letter."),
        json!("bm_george"),
        "untagged text uses the main voice"
    );
    page.locator("#vreader-stop").click().await?;

    // Ereader: spoken heading summary, intro, and an inline dialogue split into three segments.
    page.goto(&book.url).await?;
    open(&page).await?;
    page.locator("#vreader-cast")
        .wait_for(WaitState::Visible)
        .await?;
    page.locator("#vreader-voice")
        .select_option(&["bm_george"])
        .await?;
    roles.nth(1).select_option(&["pocket_alba"]).await?;
    roles.nth(2).select_option(&["pocket_javert"]).await?;
    let before = worker.request_count();
    page.locator("#vreader-play").click().await?;
    let texts = [
        "Chapter One. Part One.",
        "Previously, nothing happened.",
        "I walked out into the snow.",
        "He said,",
        "\"Wait for me,\"",
        "and vanished.",
    ];
    let seen = requests_for(worker, before, &texts).await;
    assert_eq!(
        voice_of(&seen, "Chapter One. Part One."),
        json!("bm_george"),
        "heading summary is spoken in the narrator (main) voice"
    );
    assert!(
        !seen.iter().any(|item| item.body["text"]
            .as_str()
            .is_some_and(|text| text.contains("12 words"))),
        "heading meta is not read"
    );
    assert_eq!(
        seen.iter()
            .filter(|item| item.body["text"] == json!("Previously, nothing happened."))
            .count(),
        1,
        "intro is read once"
    );
    assert_eq!(
        voice_of(&seen, "I walked out into the snow."),
        json!("pocket_alba")
    );
    assert_eq!(voice_of(&seen, "He said,"), json!("pocket_alba"));
    assert_eq!(
        voice_of(&seen, "\"Wait for me,\""),
        json!("pocket_javert"),
        "inline dialogue uses the creature voice"
    );
    assert_eq!(voice_of(&seen, "and vanished."), json!("pocket_alba"));
    page.locator("#vreader-stop").click().await?;

    // Word and passage highlights stay inside the dialogue segment, not the whole paragraph.
    // `page.frames().find(item => item !== page.mainFrame())`: the page's one iframe.
    let frame = page.frame_locator("iframe");
    page.evaluate("(() => { window.__castMessages = []; addEventListener('message', event => window.__castMessages.push(event.data)); document.querySelector('iframe').contentWindow.postMessage({ type: 'reader:extract', requestId: 'cast-segments', mode: 'page' }, '*'); })()")
        .await?;
    page.wait_for_function(
        "() => window.__castMessages.some(message => message && message.requestId === 'cast-segments')",
        &[],
    )
    .await?;
    let blocks = page
        .evaluate(
            "window.__castMessages.find(message => message.requestId === 'cast-segments').blocks",
        )
        .await?;
    let dialogue = blocks
        .as_array()
        .into_iter()
        .flatten()
        .find(|block| block["text"] == json!("\"Wait for me,\""))
        .cloned()
        .unwrap_or(Value::Null);
    assert_eq!(
        dialogue["role"],
        json!("creature"),
        "segment carries its role"
    );
    send(
        &page,
        json!({"type": "reader:highlight", "id": dialogue["id"]}),
    )
    .await?;
    frame
        .wait_for_function(
            "() => CSS.highlights.has('artifact-reader-passage')",
            Value::Null,
        )
        .await?;
    assert_eq!(
        marked(&frame, "artifact-reader-passage").await?,
        json!("\"Wait for me,\""),
        "passage highlight covers only the segment"
    );
    send(&page, json!({"type": "reader:word", "id": dialogue["id"], "text": dialogue["text"], "offset": 0, "start": 1, "end": 5}))
        .await?;
    frame
        .wait_for_function(
            "() => CSS.highlights.has('artifact-reader-word')",
            Value::Null,
        )
        .await?;
    assert_eq!(
        marked(&frame, "artifact-reader-word").await?,
        json!("Wait"),
        "word offsets map into the segment"
    );
    send(&page, json!({"type": "reader:highlight", "id": null})).await?;

    // Untagged artifact: no Voices section.
    page.goto(&plain.url).await?;
    open(&page).await?;
    assert!(page.locator("#vreader-cast").is_hidden().await?);
    Ok("PASS: cast controls, role voices, inline dialogue split, ereader heading and intro, untagged fallback".into())
}
