// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
//! `reader-mixed-content`: synthetic regression fixtures for issue #41 (document order, explicit
//! regions, hidden controls, clocks, disclosures, charts, tables and literal selection), from
//! `playwright/tests/reader-mixed-content.cjs`.

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
    /// A sandboxed `srcdoc` iframe with `html` and the bridge; wait for `reader:ready`.
    async fn setup(&self, html: &str) -> Result<(), Error> {
        self.page
            .set_content(
                "<iframe style=\"width:95vw;height:90vh\" sandbox=\"allow-scripts\"></iframe>",
            )
            .await?;
        self.page
            .call(
                "(html) => { window.messages = []; window.onmessage = (e) => messages.push(e.data); document.querySelector('iframe').srcdoc = html; }",
                &[json!(format!("{html}<script>{}</script>", self.source))],
            )
            .await?;
        self.page
            .wait_for_function("() => messages.some((m) => m.type === 'reader:ready')", &[])
            .await?;
        Ok(())
    }

    async fn send(&self, data: Value) -> Result<(), Error> {
        self.page
            .call(
                "(data) => document.querySelector('iframe').contentWindow.postMessage(data, '*')",
                &[data],
            )
            .await?;
        Ok(())
    }

    async fn extract(&mut self, mode: &str) -> Result<Value, Error> {
        let request = format!("case{}", self.serial);
        self.serial += 1;
        self.send(json!({"type": "reader:extract", "requestId": request, "mode": mode}))
            .await?;
        self.page
            .wait_for_function(
                "(id) => messages.some((m) => m.requestId === id)",
                &[json!(request)],
            )
            .await?;
        self.page
            .call(
                "(id) => messages.find((m) => m.requestId === id)",
                &[json!(request)],
            )
            .await
    }

    async fn first_message(&self, kind: &str) -> Result<Value, Error> {
        self.page
            .wait_for_function(
                "(kind) => messages.some((m) => m.type === kind)",
                &[json!(kind)],
            )
            .await?;
        self.page
            .call(
                "(kind) => messages.find((m) => m.type === kind)",
                &[json!(kind)],
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
    let frame = page.frame_locator("iframe");

    bridge
        .setup("<h1>Report</h1><p>Before prototype</p><main><h2>Prototype</h2><input value=\"secret\"><button>Apply</button><p>Visible result</p></main><p>After prototype</p><nav>Menu</nav><span role=\"timer\">00:01</span><details><summary>More</summary><p>Hidden detail</p></details>")
        .await?;
    let result = bridge.extract("page").await?;
    assert_eq!(
        texts(&result["blocks"]),
        [
            "Report",
            "Before prototype",
            "Prototype",
            "Visible result",
            "After prototype",
            "More"
        ]
    );
    frame
        .locator("[role=timer]")
        .evaluate("n => n.textContent = '00:02'", Value::Null)
        .await?;
    wait(230).await;
    assert_eq!(
        page.evaluate("messages.filter(m => m.type === 'reader:changed').length")
            .await?,
        json!(0)
    );
    frame
        .locator("details")
        .evaluate("n => n.open = true", Value::Null)
        .await?;
    page.wait_for_function("() => messages.some(m => m.type === 'reader:changed')", &[])
        .await?;
    assert!(
        texts(&bridge.extract("page").await?["blocks"])
            .iter()
            .any(|text| text == "Hidden detail")
    );

    bridge
        .setup("<h1>Outside</h1><main data-artifact-reader-region><h2>Chosen</h2><article data-artifact-reader-region><p>Once</p></article></main><p>Outside too</p>")
        .await?;
    assert_eq!(
        texts(&bridge.extract("page").await?["blocks"]),
        ["Chosen", "Once"]
    );

    bridge
        .setup("<main><figure id=\"chart\"><svg><text>100 200 300</text></svg><figcaption>Revenue increased.</figcaption></figure><figure><canvas></canvas></figure><img alt=\"\"><svg aria-hidden=\"true\"><text>Icon</text></svg><figure aria-describedby=\"explanation\"><svg><text>Axis clutter</text></svg></figure><p id=\"explanation\">Costs fell.</p><div data-artifact-reader-block><span>Availability</span><div>99.9 percent</div></div><ul><li>Label <strong>with emphasis</strong><p>and explanation</p></li></ul></main>")
        .await?;
    let result = bridge.extract("page").await?;
    assert_eq!(
        texts(&result["blocks"]),
        [
            "Revenue increased.",
            "Visual. No description is available.",
            "Costs fell.",
            "Availability 99.9 percent",
            "Label with emphasis and explanation"
        ]
    );
    bridge
        .send(json!({"type": "reader:highlight", "id": result["blocks"][0]["id"]}))
        .await?;
    assert_eq!(
        frame
            .locator("#chart")
            .get_attribute("data-artifact-reader-active")
            .await?,
        Some(String::new())
    );
    bridge
        .send(json!({"type": "reader:pick-mode", "enabled": true}))
        .await?;
    frame.locator("#chart svg").click().await?;
    assert_eq!(
        bridge.first_message("reader:target").await?["ordinal"],
        json!(0)
    );
    let block = &result["blocks"][3];
    bridge
        .send(json!({"type": "reader:word", "id": block["id"], "text": block["text"], "offset": 0, "start": 0, "end": 12}))
        .await?;
    frame
        .wait_for_function(
            "() => CSS.highlights.has('artifact-reader-word')",
            Value::Null,
        )
        .await?;
    assert_eq!(
        frame
            .evaluate(
                "() => [...CSS.highlights.get('artifact-reader-word')][0].toString()",
                Value::Null
            )
            .await?,
        json!("Availability")
    );

    bridge
        .setup("<table><caption>Quarterly results</caption><tr><th>Quarter</th><th>Revenue</th></tr><tr id=\"row\"><th>Q1</th><td>$20</td></tr><tr><th>Q2</th><td>$30</td></tr></table><pre id=\"code\">echo hello</pre>")
        .await?;
    let result = bridge.extract("page").await?;
    assert_eq!(
        texts(&result["blocks"]),
        [
            "Quarterly results",
            "Quarter: Q1. Revenue: $20",
            "Quarter: Q2. Revenue: $30",
            "Code block. Use Read details for literal reading."
        ]
    );
    bridge
        .send(json!({"type": "reader:pick-mode", "enabled": true}))
        .await?;
    frame.locator("#row td").click().await?;
    assert_eq!(
        bridge.first_message("reader:target").await?["ordinal"],
        json!(1)
    );
    frame
        .locator("#code")
        .evaluate(
            "node => { const r = document.createRange(); r.selectNodeContents(node); getSelection().removeAllRanges(); getSelection().addRange(r); }",
            Value::Null,
        )
        .await?;
    wait(50).await;
    assert_eq!(
        bridge.extract("selection").await?["blocks"][0]["text"],
        json!("echo hello")
    );

    let rows = "<tr><td>1</td><td>2</td></tr>".repeat(20);
    bridge
        .setup(&format!(
            "<table><tr><th>A</th><th>B</th></tr>{rows}</table>"
        ))
        .await?;
    assert_eq!(
        texts(&bridge.extract("page").await?["blocks"]),
        ["Table. Use Read details to hear its rows."]
    );
    bridge
        .setup("<h1>Keep</h1><section data-artifact-reader-region=\"exclude\">Never read</section><p>Keep too</p>")
        .await?;
    assert_eq!(
        texts(&bridge.extract("page").await?["blocks"]),
        ["Keep", "Keep too"]
    );
    bridge
        .setup("<ul><li>Chart context<figure><svg><text>Do not read axis</text></svg><figcaption>Sales improved.</figcaption></figure></li></ul>")
        .await?;
    assert_eq!(
        texts(&bridge.extract("page").await?["blocks"]),
        ["Chart context", "Sales improved."]
    );
    assert_eq!(errors(&page), Vec::<String>::new());
    Ok("PASS: mixed document order, explicit regions, hidden controls, clocks, disclosures, charts, descriptions, stats, lists, semantic targets, word mapping, tables, literal selection; no page errors".into())
}
