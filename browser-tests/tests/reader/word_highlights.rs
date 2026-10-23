// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
//! `reader-word-highlights`: word and passage highlights over long selections, chunk offsets,
//! several paragraphs, line breaks, nested markup, a cleared native selection, the mapping
//! fallback, stop and mutation, from `playwright/tests/reader-word-highlights.cjs`. The page
//! comes from a stub HTTP server on loopback, as the original's `http.createServer`.

use browser_tests::reader::{ReaderCtx, StubResponse, StubServer};
use browser_tests::{Error, FrameLocator, Page};
use serde_json::{Value, json};

use crate::common::{asset, errors, index_of, js_length, ordered, wait};

async fn send(page: &Page, message: Value) -> Result<(), Error> {
    page.call(
        "(m) => document.querySelector('iframe').contentWindow.postMessage(m, '*')",
        &[message],
    )
    .await?;
    Ok(())
}

async fn marked(frame: &FrameLocator<'_>, name: &str) -> Result<Value, Error> {
    frame
        .evaluate(
            "(n) => { const h = CSS.highlights.get(n); return h ? [...h].map(r => r.toString()).join('') : null; }",
            json!(name),
        )
        .await
}

pub async fn run(t: ReaderCtx) -> Result<String, Error> {
    let bridge = asset("reader-bridge.js")?;
    let text = "A quiet sentence repeats through the selected passage. ".repeat(15);
    let html = format!(
        "<main><p id=\"a\">Beginning <em>{text}</em></p><p id=\"b\">Final<br>destination and café 😀 words.</p></main><script>{bridge}</script>"
    );
    let srcdoc = serde_json::to_string(&html)
        .map_err(|error| Error::Protocol(error.to_string()))?
        .replace("</", "<\\/");
    let document = format!(
        "<iframe sandbox=\"allow-scripts\" style=\"width:95vw;height:90vh\"></iframe><script>window.messages=[];addEventListener(\"message\",e=>messages.push(e.data));document.querySelector(\"iframe\").srcdoc={srcdoc}</script>"
    );
    let server = StubServer::start(move |_request| {
        let document = document.clone();
        async move { StubResponse::new(200, "text/html; charset=utf-8", document) }
    })
    .await?;

    let page = t.page().await?;
    page.goto(&server.url()).await?;
    let frame = page.frame_locator("iframe");
    frame
        .wait_for_function(
            "() => typeof CSS.highlights?.set === 'function'",
            Value::Null,
        )
        .await?;
    frame
        .evaluate(
            "() => { document.body.focus(); const r = document.createRange(); r.selectNodeContents(document.querySelector('main')); getSelection().removeAllRanges(); getSelection().addRange(r); }",
            Value::Null,
        )
        .await?;
    wait(50).await;
    send(
        &page,
        json!({"type": "reader:extract", "requestId": "selection-1", "mode": "selection"}),
    )
    .await?;
    page.wait_for_function(
        "() => messages.some(m => m.requestId === 'selection-1')",
        &[],
    )
    .await?;
    let content = page
        .evaluate("messages.find(m => m.requestId === 'selection-1')")
        .await?;
    let whole = content["blocks"][0]["text"]
        .as_str()
        .unwrap_or("")
        .to_owned();
    assert!(js_length(&whole) > 600, "{whole:?}");
    assert!(whole.contains("Final destination"), "{whole:?}");
    send(
        &page,
        json!({"type": "reader:highlight", "id": "selection"}),
    )
    .await?;
    frame
        .wait_for_function(
            "() => CSS.highlights.has('artifact-reader-passage')",
            Value::Null,
        )
        .await?;
    // JavaScript offsets count UTF-16 units.
    let (offset, offset_byte) = index_of(&whole, "Final destination").unwrap_or((0, 0));
    let chunk = whole[offset_byte..].to_owned();
    send(&page, json!({"type": "reader:word", "id": "selection", "text": chunk, "offset": offset, "start": 6, "end": 17}))
        .await?;
    frame
        .wait_for_function(
            "() => CSS.highlights.has('artifact-reader-word')",
            Value::Null,
        )
        .await?;
    assert_eq!(
        marked(&frame, "artifact-reader-word").await?,
        json!("destination")
    );
    assert_eq!(
        marked(&frame, "artifact-reader-passage").await?,
        Value::Null
    );
    assert_eq!(
        frame
            .evaluate("() => getSelection().toString()", Value::Null)
            .await?,
        json!("")
    );
    // Clearing native selection while the iframe owns focus must not erase the playback map.
    wait(50).await;
    let (cafe, _) = index_of(&chunk, "café").unwrap_or((usize::MAX, 0));
    send(&page, json!({"type": "reader:word", "id": "selection", "text": chunk, "offset": offset, "start": cafe, "end": cafe + 4}))
        .await?;
    wait(50).await;
    assert_eq!(marked(&frame, "artifact-reader-word").await?, json!("café"));
    send(&page, json!({"type": "reader:word", "id": "selection", "text": "mismatched text", "offset": 0, "start": 0, "end": 3}))
        .await?;
    frame
        .wait_for_function(
            "() => !CSS.highlights.has('artifact-reader-word')",
            Value::Null,
        )
        .await?;
    assert!(crate::common::truthy(
        &marked(&frame, "artifact-reader-passage").await?
    ));
    send(&page, json!({"type": "reader:highlight", "id": null})).await?;
    frame
        .wait_for_function("() => CSS.highlights.size === 0", Value::Null)
        .await?;
    send(
        &page,
        json!({"type": "reader:extract", "requestId": "page-1", "mode": "page"}),
    )
    .await?;
    page.wait_for_function("() => messages.some(m => m.requestId === 'page-1')", &[])
        .await?;
    let block = page
        .evaluate("messages.find(m => m.requestId === 'page-1').blocks.find(b => b.text.startsWith('Final'))")
        .await?;
    send(
        &page,
        json!({"type": "reader:highlight", "id": block["id"]}),
    )
    .await?;
    send(&page, json!({"type": "reader:word", "id": block["id"], "text": block["text"], "offset": 0, "start": 6, "end": 17}))
        .await?;
    frame
        .wait_for_function(
            "() => CSS.highlights.has('artifact-reader-word')",
            Value::Null,
        )
        .await?;
    assert_eq!(
        marked(&frame, "artifact-reader-word").await?,
        json!("destination")
    );
    frame
        .evaluate(
            "() => { document.querySelector('#b').firstChild.data = 'Changed'; }",
            Value::Null,
        )
        .await?;
    frame
        .wait_for_function("() => CSS.highlights.size === 0", Value::Null)
        .await?;
    let page_errors = errors(&page);
    assert_eq!(page_errors, Vec::<String>::new());
    Ok(ordered(&[
        ("longSelection", json!(true)),
        ("chunkOffsets", json!(true)),
        ("multipleParagraphs", json!(true)),
        ("lineBreak", json!(true)),
        ("nestedMarkup", json!(true)),
        ("nativeSelectionClear", json!(true)),
        ("mappingFallback", json!(true)),
        ("stop", json!(true)),
        ("mutation", json!(true)),
        ("errors", json!(page_errors)),
    ]))
}
