// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
//! `reader-sections`: the bridge with the ereader adapter (TOC seek, fingerprint rejection,
//! section bounds), from `playwright/tests/reader-sections.cjs`. Messages reach the test
//! through an exposed function, as in the original.

use std::sync::{Arc, Mutex};

use browser_tests::reader::ReaderCtx;
use browser_tests::{Error, Page};
use serde_json::{Value, json};

use crate::common::{asset, errors, texts, wait};

type Messages = Arc<Mutex<Vec<Value>>>;

async fn post(page: &Page, message: Value) -> Result<(), Error> {
    page.call(
        "(message) => document.getElementById('book').contentWindow.postMessage(message, '*')",
        &[message],
    )
    .await?;
    Ok(())
}

fn find(messages: &Messages, kind: &str, request: &str) -> Option<Value> {
    messages
        .lock()
        .unwrap()
        .iter()
        .find(|message| message["type"] == json!(kind) && message["requestId"] == json!(request))
        .cloned()
}

fn fail(message: impl Into<String>) -> Error {
    Error::Assertion(message.into())
}

pub async fn run(t: ReaderCtx) -> Result<String, Error> {
    let bridge = asset("reader-bridge.js")?;
    let ereader = asset("reader-ereader.js")?;
    let page = t.page().await?;
    let messages: Messages = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&messages);
    page.expose_function("captureReaderMessage", move |args| {
        sink.lock()
            .unwrap()
            .push(args.into_iter().next().unwrap_or(Value::Null));
        Ok(Value::Null)
    })
    .await?;
    page.set_content("<iframe id=\"book\"></iframe>").await?;
    let frame = page.frame_locator("#book");
    page.evaluate("window.addEventListener('message', e => window.captureReaderMessage(e.data))")
        .await?;
    let book = json!({"chapters": [
        {"part": "Part One", "title": "Chapter 1", "blocks": [{"t": "p", "s": "First chapter text."}]},
        {"part": "Part One", "title": "Chapter 2", "blocks": [{"t": "p", "s": "Second chapter text."}]},
    ]});
    let toc_script = "document.addEventListener('click',function(e){var go=e.target.closest('[data-go]');if(!go)return;e.preventDefault();var i=+go.getAttribute('data-go'),c=CH[i];var cover=document.getElementById('cover');if(!cover)return;cover.outerHTML='<article id=\"article\"><header class=\"ch-head\"><div class=\"part\">'+c.part+'</div><h1>'+c.title+'</h1></header><p data-p=\"0\">'+c.blocks[0].s+'</p></article>';});document.getElementById('t-contents').addEventListener('click',function(){document.querySelector('.toc').hidden=false;});";
    frame
        .set_content(&format!(
            "<script id=\"book\" type=\"application/json\">{book}</script><button id=\"t-contents\" data-panel=\"contents\">Contents</button><div class=\"toc\" hidden><a href=\"#\" data-go=\"0\">One</a><a href=\"#\" data-go=\"1\">Two</a></div><main id=\"cover\"><h1>Cover</h1><p>Cover text.</p></main><script>{ereader}</script><script>{toc_script}</script><script>{bridge}</script>"
        ))
        .await?;
    frame
        .evaluate(
            "chapters => { window.CH = chapters; }",
            book["chapters"].clone(),
        )
        .await?;
    post(
        &page,
        json!({"type": "reader:resume", "requestId": "seek", "mode": "page", "chapterIndex": 1, "fingerprint": "wrong"}),
    )
    .await?;
    wait(150).await;
    if find(&messages, "reader:error", "seek").is_none() {
        return Err(fail("cover seek/change rejection failed"));
    }
    messages.lock().unwrap().clear();
    post(
        &page,
        json!({"type": "reader:extract", "requestId": "page", "mode": "page"}),
    )
    .await?;
    wait(80).await;
    let result = find(&messages, "reader:content", "page");
    let Some(result) = result.filter(|result| {
        result["chapter"]["index"] == json!(1) && crate::common::truthy(&result["fingerprint"])
    }) else {
        return Err(fail("TOC seek fixture failed"));
    };
    messages.lock().unwrap().clear();
    frame
        .locator("p[data-p]")
        .evaluate(
            "n => { n.textContent = 'Changed chapter text.'; }",
            Value::Null,
        )
        .await?;
    post(
        &page,
        json!({"type": "reader:resume", "requestId": "changed", "mode": "page", "chapterIndex": 1, "fingerprint": result["fingerprint"]}),
    )
    .await?;
    wait(100).await;
    if find(&messages, "reader:error", "changed").is_none() {
        return Err(fail("changed DOM resume was accepted"));
    }
    messages.lock().unwrap().clear();
    frame
        .locator("p[data-p]")
        .evaluate(
            "(n, text) => { n.textContent = text; }",
            book["chapters"][1]["blocks"][0]["s"].clone(),
        )
        .await?;
    post(
        &page,
        json!({"type": "reader:extract", "requestId": "section", "mode": "section"}),
    )
    .await?;
    wait(80).await;
    let section = find(&messages, "reader:content", "section");
    let bounded = section.as_ref().is_some_and(|section| {
        section["blocks"].as_array().map(Vec::len) == Some(2)
            && section["blocks"][0]["sectionEndOrdinal"] == json!(1)
    });
    if !bounded {
        return Err(fail(format!(
            "section boundary fixture failed: {section:?}"
        )));
    }
    frame
        .set_content(&format!("<main><section><h1>Outer</h1><p>Outer intro</p><section><h2>Inner</h2><p id=\"inner\">Inner text</p></section></section><section><h2>Sibling</h2><p>Sibling text</p></section></main><script>{bridge}</script>"))
        .await?;
    messages.lock().unwrap().clear();
    frame
        .locator("#inner")
        .dispatch_event("pointerdown")
        .await?;
    post(
        &page,
        json!({"type": "reader:extract", "requestId": "nested", "mode": "section"}),
    )
    .await?;
    wait(100).await;
    let nested = find(&messages, "reader:content", "nested");
    if nested.as_ref().map(|nested| texts(&nested["blocks"]))
        != Some(vec!["Inner".into(), "Inner text".into()])
    {
        return Err(fail(format!(
            "Nested section leaks outer or sibling text: {nested:?}"
        )));
    }
    frame
        .set_content(&format!("<main><h1>One</h1><p>First</p><h2>Two</h2><p id=\"two\">Second</p><h2>Three</h2><p>Third</p></main><script>{bridge}</script>"))
        .await?;
    messages.lock().unwrap().clear();
    frame.locator("#two").dispatch_event("pointerdown").await?;
    post(
        &page,
        json!({"type": "reader:extract", "requestId": "headings", "mode": "section"}),
    )
    .await?;
    wait(100).await;
    let headings = find(&messages, "reader:content", "headings");
    if headings.as_ref().map(|headings| texts(&headings["blocks"]))
        != Some(vec!["Two".into(), "Second".into()])
    {
        return Err(fail(format!("Heading section scope wrong: {headings:?}")));
    }
    let page_errors = errors(&page);
    if !page_errors.is_empty() {
        return Err(fail(format!(
            "reader section page errors: {}",
            page_errors.join("; ")
        )));
    }
    Ok(
        "reader bridge browser fixtures passed: TOC seek, fingerprint rejection, section bounds"
            .into(),
    )
}
