// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
//! `reader-scopes`: synthetic browser contract tests for mixed-content reading scopes, from
//! `playwright/tests/reader-scopes.cjs`.

use browser_tests::reader::ReaderCtx;
use browser_tests::{Error, Page};
use serde_json::{Value, json};

use crate::common::{asset, errors, texts, wait};

struct Bridge<'p> {
    page: &'p Page,
    source: String,
    serial: usize,
}

impl Bridge<'_> {
    async fn setup(&self, html: &str) -> Result<(), Error> {
        self.page
            .set_content("<iframe id=\"reader\" style=\"width:95vw;height:90vh\" sandbox=\"allow-scripts\"></iframe>")
            .await?;
        self.page
            .call(
                "(source) => { window.readerMessages = []; window.addEventListener('message', event => window.readerMessages.push(event.data)); document.querySelector('#reader').srcdoc = source; }",
                &[json!(format!("{html}<script>{}</script>", self.source))],
            )
            .await?;
        self.page
            .wait_for_function(
                "() => window.readerMessages.some(message => message.type === 'reader:ready')",
                &[],
            )
            .await?;
        Ok(())
    }

    async fn send(&self, data: Value) -> Result<(), Error> {
        self.page
            .call(
                "(data) => document.querySelector('#reader').contentWindow.postMessage(data, '*')",
                &[data],
            )
            .await?;
        Ok(())
    }

    async fn message_for(&self, request: &str) -> Result<Value, Error> {
        self.page
            .wait_for_function(
                "(id) => window.readerMessages.some(message => message.requestId === id)",
                &[json!(request)],
            )
            .await?;
        self.page
            .call(
                "(id) => window.readerMessages.find(message => message.requestId === id)",
                &[json!(request)],
            )
            .await
    }

    fn next_request(&mut self) -> String {
        let request = format!("scope-{}", self.serial);
        self.serial += 1;
        request
    }

    async fn extract(&mut self, mode: &str, extra: Value) -> Result<Value, Error> {
        let request = self.next_request();
        let mut message = json!({"type": "reader:extract", "requestId": request, "mode": mode});
        if let (Some(message), Value::Object(extra)) = (message.as_object_mut(), extra) {
            message.extend(extra);
        }
        self.send(message).await?;
        self.message_for(&request).await
    }

    async fn clear_messages(&self) -> Result<(), Error> {
        self.page
            .evaluate("window.readerMessages.length = 0")
            .await?;
        Ok(())
    }

    async fn changed_count(&self) -> Result<Value, Error> {
        self.page
            .evaluate(
                "window.readerMessages.filter(message => message.type === 'reader:changed').length",
            )
            .await
    }
}

pub async fn run(t: ReaderCtx) -> Result<String, Error> {
    let page = t.page().await?;
    let mut bridge = Bridge {
        page: &page,
        source: asset("reader-bridge.js")?,
        serial: 0,
    };
    let frame = page.frame_locator("#reader");

    bridge
        .setup(
            r#"
      <main>
        <h1>Outside heading</h1><p id="outside">Outside text</p>
        <section id="view-a" role="tabpanel" aria-label="First view">
          <h2>First view</h2><p id="a-text">First view text.</p>
          <table id="small-table"><tr><th>Metric</th><th>Value</th></tr><tr id="row-a"><td>Users</td><td>42</td></tr></table>
        </section>
        <section id="view-b" role="tabpanel" hidden><h2>Second view</h2><p>Hidden view text.</p></section>
      </main>"#,
        )
        .await?;

    let view = bridge
        .extract("view", json!({"scopeKey": "id:view-a"}))
        .await?;
    assert_eq!(view["type"], json!("reader:content"));
    assert_eq!(view["mode"], json!("view"));
    assert_eq!(view["scopeKey"], json!("id:view-a"));
    assert_eq!(
        texts(&view["blocks"]),
        ["First view", "First view text.", "Metric: Users. Value: 42"]
    );
    assert!(
        crate::common::truthy(&view["fingerprint"]),
        "view response should include a fingerprint"
    );

    frame
        .locator("#a-text")
        .dispatch_event("pointerdown")
        .await?;
    let section = bridge.extract("section", json!({})).await?;
    assert!(
        crate::common::truthy(&section["scopeKey"]),
        "section response should include a resumable scope key"
    );
    assert_eq!(
        texts(&section["blocks"]),
        ["First view", "First view text.", "Metric: Users. Value: 42"]
    );

    let request = bridge.next_request();
    bridge
        .send(json!({"type": "reader:resume", "requestId": request, "mode": "view", "scopeKey": view["scopeKey"], "fingerprint": view["fingerprint"]}))
        .await?;
    let resumed = bridge.message_for(&request).await?;
    assert_eq!(resumed["type"], json!("reader:content"));
    assert_eq!(resumed["scopeKey"], view["scopeKey"]);
    assert_eq!(resumed["fingerprint"], view["fingerprint"]);

    bridge.clear_messages().await?;
    frame
        .locator("#outside")
        .evaluate(
            "node => { node.textContent = 'Outside changed but excluded.'; }",
            Value::Null,
        )
        .await?;
    wait(300).await;
    assert_eq!(
        bridge.changed_count().await?,
        json!(0),
        "mutation outside the active view must not invalidate playback"
    );

    frame
        .locator("#a-text")
        .evaluate(
            "node => { node.textContent = 'First view changed.'; }",
            Value::Null,
        )
        .await?;
    page.wait_for_function(
        "() => window.readerMessages.some(message => message.type === 'reader:changed')",
        &[],
    )
    .await?;

    let hidden_request = bridge.next_request();
    frame
        .locator("#view-a")
        .evaluate("node => { node.hidden = true; }", Value::Null)
        .await?;
    bridge
        .send(json!({"type": "reader:resume", "requestId": hidden_request, "mode": "view", "scopeKey": view["scopeKey"], "fingerprint": view["fingerprint"]}))
        .await?;
    let hidden = bridge.message_for(&hidden_request).await?;
    assert_eq!(
        hidden["type"],
        json!("reader:error"),
        "hidden views must refuse resume"
    );

    bridge
        .setup(
            r#"
      <main>
        <section id="tabs" role="tablist"><button role="tab" aria-selected="true">Tab A</button><button role="tab">Tab B</button></section>
        <section id="active" role="tabpanel"><h2>Active tab panel</h2><p>Panel content.</p></section>
        <section id="inactive" role="tabpanel" hidden><p>Inactive panel.</p></section>
      </main>"#,
        )
        .await?;
    let active_view = bridge.extract("view", json!({})).await?;
    assert_eq!(active_view["scopeKey"], json!("id:active"));
    assert_eq!(
        texts(&active_view["blocks"]),
        ["Active tab panel", "Panel content."]
    );
    assert_eq!(
        bridge.changed_count().await?,
        json!(0),
        "view extraction should not require a click to identify the active tab panel"
    );

    let rows: String = (0..20)
        .map(|index| {
            format!(
                "<tr id=\"sales-{index}\"><td>Region {}</td><td>{}</td></tr>",
                index + 1,
                (index + 1) * 10
            )
        })
        .collect();
    bridge
        .setup(&format!(
            r#"
      <main>
        <table id="large"><caption>Sales by region</caption>
          <tr><th>Region</th><th>Sales</th></tr>
          {rows}
        </table>
        <pre id="code">const answer = 42;
console.log(answer);</pre>
        <div id="visual" data-artifact-reader-detail="A scatter plot showing sales rising over time."><svg aria-label="plot"><text>axis labels</text></svg></div>
      </main>"#
        ))
        .await?;
    let large = bridge.extract("page", json!({})).await?;
    assert_eq!(
        texts(&large["blocks"]),
        [
            "Sales by region",
            "Code block. Use Read details for literal reading.",
            "plot"
        ]
    );

    let table_detail = bridge
        .extract("detail", json!({"scopeKey": "id:large"}))
        .await?;
    assert_eq!(table_detail["scopeKey"], json!("id:large"));
    assert_eq!(table_detail["blocks"][0]["text"], json!("Sales by region"));
    let detail_blocks = table_detail["blocks"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert_eq!(
        detail_blocks.len(),
        21,
        "large-table details should include caption and all visible data rows"
    );
    assert_eq!(
        detail_blocks.last().map(|block| block["text"].clone()),
        Some(json!("Region: Region 20. Sales: 200"))
    );

    let code_detail = bridge
        .extract("detail", json!({"scopeKey": "id:code"}))
        .await?;
    assert_eq!(
        code_detail["blocks"][0]["text"],
        json!("const answer = 42; console.log(answer);")
    );

    let visual_detail = bridge
        .extract("detail", json!({"scopeKey": "id:visual"}))
        .await?;
    assert_eq!(
        visual_detail["blocks"][0]["text"],
        json!("A scatter plot showing sales rising over time.")
    );

    bridge
        .send(json!({"type": "reader:pick-mode", "enabled": true}))
        .await?;
    frame.locator("#sales-7 td").first().click().await?;
    page.wait_for_function(
        "() => window.readerMessages.some(message => message.type === 'reader:target')",
        &[],
    )
    .await?;
    let target = page
        .evaluate("window.readerMessages.find(message => message.type === 'reader:target')")
        .await?;
    assert_eq!(target["detailAvailable"], json!(true));
    assert_eq!(target["scopeKey"], json!("id:sales-7"));
    assert_eq!(
        target["ordinal"],
        json!(0),
        "selected detail row should map to the collapsed table reading block"
    );

    let path = frame
        .locator("#sales-7")
        .evaluate(
            "node => { const indexes = []; for (let current = node; current && current !== document.documentElement; current = current.parentElement) indexes.unshift([...current.parentElement.children].indexOf(current)); return indexes.join('.'); }",
            Value::Null,
        )
        .await?;
    let selected_row = bridge
        .extract(
            "detail",
            json!({"scopeKey": format!("path:{}", path.as_str().unwrap_or(""))}),
        )
        .await?;
    assert_eq!(
        selected_row["blocks"][0]["text"],
        json!("Region: Region 8. Sales: 80")
    );
    assert_eq!(
        errors(&page),
        Vec::<String>::new(),
        "reader scope fixtures should not produce page errors"
    );
    Ok("PASS: view/detail/section scopes, scoped resume fingerprints, mutation invalidation, hidden-view rejection, active tab selection, explicit large-table/code/visual details, selected table row".into())
}
