// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
//! `artifact viewer`, from `playwright/tests/02-viewer.spec.mjs`.

use browser_tests::e2e::{Case, Ctx, Identity, encode_component, now_millis};
use browser_tests::{Error, LogEntry, Page, Role, e2e_case, expect, poll, regex};
use serde_json::{Value, json};

const SPEC: &str = "02-viewer";

pub fn cases() -> Vec<Case> {
    vec![
        e2e_case!(
            SPEC,
            "viewer identity and private state stay isolated while org state is shared",
            identity_and_private_state
        ),
        e2e_case!(
            SPEC,
            "shell saves state across reload and another browser in the same organization",
            state_across_reload_and_browsers
        ),
        e2e_case!(
            SPEC,
            "shell renders, iframe is sandboxed without allow-same-origin",
            iframe_is_sandboxed
        ),
        e2e_case!(
            SPEC,
            "same-document artifact links do not show the external-link confirmation",
            same_document_links
        ),
        e2e_case!(
            SPEC,
            "viewer chrome keeps menus, focus, and mobile targets predictable",
            viewer_chrome
        ),
        e2e_case!(
            SPEC,
            "anchored comment composer keeps a v2 draft separate from prompt copy",
            anchored_composer_draft
        ),
        e2e_case!(
            SPEC,
            "anchored composer uses bridge pixels and stays clamped at desktop, tablet, and mobile sizes",
            anchored_composer_clamped
        ),
        e2e_case!(
            SPEC,
            "raw delivery carries the CSP sandbox and never allow-same-origin",
            raw_csp_sandbox
        ),
        e2e_case!(SPEC, "download returns an attachment", download_attachment),
        e2e_case!(
            SPEC,
            "history lists revisions after an update",
            history_after_update
        ),
        e2e_case!(
            SPEC,
            "discussion details expose guarded policy controls without an untrusted thread link",
            discussion_details
        ),
        e2e_case!(
            SPEC,
            "an artifact owner can manage discussion status while a same-org non-owner cannot",
            discussion_owner_only
        ),
    ]
}

fn id_of(artifact: &Value) -> String {
    artifact["id"].as_str().unwrap_or("").to_owned()
}

fn parse_json(text: Option<String>) -> Result<Value, Error> {
    let text = text.unwrap_or_default();
    serde_json::from_str(&text).map_err(|error| Error::Protocol(format!("{text:?}: {error}")))
}

/// Console errors (Playwright's `console` events of type `error`), plus uncaught exceptions
/// (`pageerror`) when `with_page_errors`, without the expected `status of 503` resource errors.
fn console_errors(page: &Page, with_page_errors: bool) -> Vec<String> {
    page.log()
        .into_iter()
        .filter_map(|entry| match entry {
            LogEntry::Console { level, text, .. } if level == "error" => Some(text),
            LogEntry::Exception { text, .. } if with_page_errors => Some(text),
            _ => None,
        })
        .filter(|message| !message.contains("status of 503"))
        .collect()
}

fn is_viewer_handle(id: &str) -> bool {
    // `/^[a-f0-9]{16}$/`
    id.len() == 16
        && id
            .bytes()
            .all(|byte| matches!(byte, b'a'..=b'f' | b'0'..=b'9'))
}

fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64().is_some_and(|n| n != 0.0),
        Value::String(text) => !text.is_empty(),
        _ => true,
    }
}

const SCOPED_STATE_HTML: &str = r#"<!doctype html><body>
        <output id="ready"></output><output id="private">Loading</output><output id="shared">Loading</output>
        <input id="note" aria-label="Note"><button id="save-private">Save private note</button><button id="save-shared">Share note</button>
        <script>
          addEventListener('message', event => {
            if (event.source !== parent) return;
            const m = event.data;
            if (m?.type === 'state:ready' && m.enabled) {
              document.querySelector('#ready').textContent = JSON.stringify(m);
              for (const scope of ['viewer','org']) parent.postMessage({type:'state:get',scope,key:'note'},'*');
            }
            if (m?.type === 'state:value') document.querySelector(m.scope === 'viewer' ? '#private' : '#shared').textContent = m.value ?? 'Empty';
            if (m?.type === 'state:saved') document.body.setAttribute('data-saved-' + m.scope, String(m.revision));
            if (m?.type === 'state:error') document.body.setAttribute('data-state-error', m.reason);
          });
          document.querySelector('#save-private').onclick = () => parent.postMessage({type:'state:set',scope:'viewer',key:'note',value:document.querySelector('#note').value},'*');
          document.querySelector('#save-shared').onclick = () => parent.postMessage({type:'state:set',scope:'org',key:'note',value:document.querySelector('#note').value},'*');
          parent.postMessage({type:'state:hello'},'*');
        </script></body>"#;

async fn identity_and_private_state(t: Ctx) -> Result<(), Error> {
    let key = t.publisher_key().await?;
    let org = &t.org;
    let emails = [
        format!("scope-one-{org}@example.test"),
        format!("scope-two-{org}@example.test"),
    ];
    let names = ["First Reader", "Second Reader"];
    for index in 0..emails.len() {
        let added = t
            .api(
                "POST",
                &format!("/settings/orgs/{org}/emails"),
                Some(json!({"email": emails[index], "display_name": names[index]})),
            )
            .await?;
        assert_eq!(added.status, 200, "{}", added.text());
    }
    let artifact = t
        .publish(
            &key,
            &format!("PW Scoped State {org}"),
            SCOPED_STATE_HTML,
            None,
        )
        .await?;
    let id = id_of(&artifact);
    let mut pages = Vec::new();
    let mut handles = Vec::new();
    for index in 0..emails.len() {
        let page = t.page_as(Identity::email(&emails[index])).await?;
        page.goto(&format!("{}/{id}", t.base_url())).await?;
        let frame = page.frame_locator("#vframe");
        expect(&frame.locator("#ready"))
            .to_contain_text(names[index])
            .await?;
        let ready = parse_json(frame.locator("#ready").text_content().await?)?;
        assert_eq!(ready["scopes"], json!(["org", "viewer"]));
        let handle = ready["viewer"]["id"].as_str().unwrap_or("").to_owned();
        assert!(is_viewer_handle(&handle), "{handle:?}");
        assert_eq!(ready["viewer"]["name"], json!(names[index]));
        handles.push(handle);
        expect(&frame.locator("#private"))
            .to_have_text("Empty")
            .await?;
        frame
            .get_by_role(Role::new("textbox").exact_name("Note"))
            .fill(&format!("Diary {index}"))
            .await?;
        frame
            .get_by_role(Role::new("button").name("Save private note"))
            .click()
            .await?;
        expect(&frame.locator("body"))
            .to_have_attribute("data-saved-viewer", "1")
            .await?;
        pages.push(page);
    }
    assert_ne!(handles[0], handles[1]);
    let first_frame = pages[0].frame_locator("#vframe");
    first_frame
        .get_by_role(Role::new("textbox").exact_name("Note"))
        .fill("Shared reading note")
        .await?;
    first_frame
        .get_by_role(Role::new("button").name("Share note"))
        .click()
        .await?;
    expect(&first_frame.locator("body"))
        .to_have_attribute("data-saved-org", "1")
        .await?;
    for (index, page) in pages.iter().enumerate() {
        page.reload().await?;
        let frame = page.frame_locator("#vframe");
        expect(&frame.locator("#private"))
            .to_have_text(format!("Diary {index}"))
            .await?;
        expect(&frame.locator("#shared"))
            .to_have_text("Shared reading note")
            .await?;
        let ready = parse_json(frame.locator("#ready").text_content().await?)?;
        assert_eq!(ready["viewer"]["id"], json!(handles[index]));
        // `toContainEqual`: one entry deep-equals the expected object.
        let expected = json!({"key": "note", "revision": 1});
        assert!(
            ready["viewerKeys"]
                .as_array()
                .is_some_and(|keys| keys.contains(&expected)),
            "{}",
            ready["viewerKeys"]
        );
        let document = frame
            .locator("html")
            .evaluate("(element) => element.outerHTML", Value::Null)
            .await?;
        let document = document.as_str().unwrap_or("");
        for email in &emails {
            assert!(
                !document.contains(email.as_str()),
                "the frame shows {email}"
            );
        }
        expect(&frame.locator("body"))
            .not()
            .to_have_attribute("data-state-error", regex(".+", ""))
            .await?;
    }
    Ok(())
}

async fn state_across_reload_and_browsers(t: Ctx) -> Result<(), Error> {
    let key = t.publisher_key().await?;
    let org = &t.org;
    let emails = [
        format!("state-one-{org}@example.test"),
        format!("state-two-{org}@example.test"),
    ];
    for email in &emails {
        let added = t
            .api(
                "POST",
                &format!("/settings/orgs/{org}/emails"),
                Some(json!({"email": email})),
            )
            .await?;
        assert_eq!(added.status, 200, "{}", added.text());
    }
    let artifact = t
        .publish(
            &key,
            &format!("PW State {org}"),
            r#"<!doctype html><body><button id="save">Save note</button><output id="value"></output><script>
        addEventListener('message', event => {
          if (event.source !== parent) return;
          const m = event.data;
          if (m?.type === 'state:ready') { document.body.dataset.enabled = String(m.enabled); if (m.enabled) parent.postMessage({type:'state:get',key:'note'},'*'); }
          if (m?.type === 'state:value') document.querySelector('#value').textContent = m.value ?? '';
          if (m?.type === 'state:saved') document.body.dataset.saved = String(m.revision);
        });
        document.querySelector('#save').onclick = () => parent.postMessage({type:'state:set',key:'note',value:'persisted'},'*');
        parent.postMessage({type:'state:hello'},'*');
      </script></body>"#,
            None,
        )
        .await?;
    let id = id_of(&artifact);
    let page = t.page_as(Identity::email(&emails[0])).await?;
    page.goto(&format!("{}/{id}", t.base_url())).await?;
    expect(&page.frame_locator("#vframe").locator("body"))
        .to_have_attribute("data-enabled", "true")
        .await?;
    page.frame_locator("#vframe")
        .get_by_role(Role::new("button").name("Save note"))
        .click()
        .await?;
    expect(&page.frame_locator("#vframe").locator("body"))
        .to_have_attribute("data-saved", "1")
        .await?;
    page.reload().await?;
    expect(&page.frame_locator("#vframe").locator("#value"))
        .to_have_text("persisted")
        .await?;
    let other = t.page_as(Identity::email(&emails[1])).await?;
    other.goto(&format!("{}/{id}", t.base_url())).await?;
    expect(&other.frame_locator("#vframe").locator("#value"))
        .to_have_text("persisted")
        .await?;
    let state = t.request().get(&format!("/{id}/state/note")).await?;
    assert_eq!(state.json()["revision"], json!(1));
    Ok(())
}

async fn iframe_is_sandboxed(t: Ctx) -> Result<(), Error> {
    let key = t.publisher_key().await?;
    let page = t.page().await?;
    let artifact = t
        .publish(
            &key,
            &format!("PW Shell {}", t.org),
            "<!doctype html><h1>shell</h1>",
            None,
        )
        .await?;
    page.goto(&format!("/{}", id_of(&artifact))).await?;
    let frame = page.locator("#vframe");
    expect(&frame).to_be_visible().await?;
    let sandbox = frame.get_attribute("sandbox").await?.unwrap_or_default();
    assert!(!sandbox.is_empty(), "iframe must be sandboxed");
    assert!(!sandbox.contains("allow-same-origin"), "{sandbox}");
    Ok(())
}

async fn same_document_links(t: Ctx) -> Result<(), Error> {
    let key = t.publisher_key().await?;
    let page = t.page().await?;
    let artifact = t
        .publish(
            &key,
            &format!("PW Internal Link {}", t.org),
            r##"<!doctype html><a href="#details">Jump to details</a><div id="details">Details</div>"##,
            None,
        )
        .await?;
    page.goto(&format!("/{}", id_of(&artifact))).await?;
    let artifact_frame = page
        .frames()
        .into_iter()
        .find(|frame| frame.id != page.main_frame_id())
        .map(|frame| frame.id);
    assert!(artifact_frame.is_some(), "the artifact frame exists");
    let artifact_frame = artifact_frame.unwrap_or_default();

    page.frame_locator("#vframe")
        .get_by_role(Role::new("link").name("Jump to details"))
        .click()
        .await?;

    poll(
        "the artifact frame URL contains #details",
        || async { Ok(page.frame_url(&artifact_frame).unwrap_or_default()) },
        |url| url.contains("#details"),
    )
    .await?;
    expect(&page.get_by_role(Role::new("dialog").name("Confirm external link")))
        .to_have_count(0)
        .await
}

async fn viewer_chrome(t: Ctx) -> Result<(), Error> {
    let key = t.publisher_key().await?;
    let page = t.page().await?;
    let artifact = t
        .publish(
            &key,
            &format!("PW Chrome {}", t.org),
            "<!doctype html><h1>chrome</h1>",
            None,
        )
        .await?;
    let id = id_of(&artifact);

    for (width, height) in [(1440, 900), (768, 1024), (390, 844)] {
        page.set_viewport(width, height, 1.0).await?;
        page.goto(&format!("/{id}")).await?;

        let title = page.locator("#vtitle-toggle");
        let title_menu = page.get_by_role(Role::new("menu").name("Artifact overview"));
        let more = page.get_by_role(Role::new("button").name("More artifact actions"));
        let more_menu = page.get_by_role(Role::new("menu").name("More artifact actions"));
        let comment = page.get_by_role(Role::new("button").name("Comment on a place"));
        let inspector = page.locator("#vinspector");
        let focus = "(button) => button.focus()";
        let contains_focus = "(panel) => panel.contains(document.activeElement)";

        expect(&inspector).to_have_attribute("inert", "").await?;
        inspector
            .locator("#vinspector-close")
            .evaluate(focus, Value::Null)
            .await?;
        assert!(!truthy(
            &inspector.evaluate(contains_focus, Value::Null).await?
        ));

        title.press("Enter").await?;
        expect(&title_menu).to_be_visible().await?;
        title.press("ArrowDown").await?;
        expect(&page.get_by_role(Role::new("menuitem").name("Details")))
            .to_be_focused()
            .await?;
        page.keyboard().press("End").await?;
        expect(&page.get_by_role(Role::new("menuitemcheckbox").name("Mark as needing work")))
            .to_be_focused()
            .await?;
        page.keyboard().press("Escape").await?;
        expect(&title).to_be_focused().await?;

        more.press("Enter").await?;
        expect(&more_menu).to_be_visible().await?;
        more.press("ArrowDown").await?;
        expect(&more_menu.get_by_role(Role::new("menuitem").name("Open raw artifact")))
            .to_be_focused()
            .await?;
        page.keyboard().press("Escape").await?;
        expect(&more).to_be_focused().await?;

        title.press("Enter").await?;
        title.press("ArrowDown").await?;
        page.get_by_role(Role::new("menuitem").name("Details"))
            .click()
            .await?;
        expect(&inspector)
            .not()
            .to_have_attribute("inert", "")
            .await?;
        page.get_by_role(Role::new("tab").name("History"))
            .click()
            .await?;
        page.get_by_role(Role::new("button").name("Close inspector"))
            .click()
            .await?;
        expect(&inspector).to_have_attribute("inert", "").await?;
        expect(&title).to_be_focused().await?;
        inspector
            .locator("#vinspector-close")
            .evaluate(focus, Value::Null)
            .await?;
        assert!(!truthy(
            &inspector.evaluate(contains_focus, Value::Null).await?
        ));

        comment.click().await?;
        expect(&comment)
            .to_have_attribute("aria-pressed", "true")
            .await?;
        page.keyboard().press("Escape").await?;
        expect(&comment)
            .to_have_attribute("aria-pressed", "false")
            .await?;
        expect(&comment).to_be_focused().await?;

        let scroll_width = page
            .evaluate("document.documentElement.scrollWidth")
            .await?;
        assert!(
            scroll_width.as_f64().unwrap_or(f64::MAX) <= f64::from(width),
            "{scroll_width}"
        );
        if width <= 760 {
            for target in [
                title.clone(),
                page.get_by_role(Role::new("link").name("Back to artifact library")),
                page.get_by_role(Role::new("button").name("Save to favorites")),
                comment.clone(),
                page.get_by_role(Role::new("button").name("Share")),
                more.clone(),
            ] {
                let bounds = target.bounding_box().await?;
                let bounds = bounds.unwrap_or_else(|| panic!("{} has no box", target.describe()));
                assert!(bounds.width >= 44.0, "{}: {bounds:?}", target.describe());
                assert!(bounds.height >= 44.0, "{}: {bounds:?}", target.describe());
            }
        }
    }

    assert_eq!(console_errors(&page, true), Vec::<String>::new());
    Ok(())
}

async fn anchored_composer_draft(t: Ctx) -> Result<(), Error> {
    let key = t.publisher_key().await?;
    let page = t.page().await?;
    let artifact = t
        .publish(
            &key,
            &format!("PW Anchor {}", t.org),
            "<!doctype html><main><p>anchor target</p></main>",
            None,
        )
        .await?;
    let id = id_of(&artifact);
    page.set_viewport(390, 844, 1.0).await?;
    page.goto(&format!("/{id}")).await?;
    let frame = page.frame_locator("#vframe");
    frame
        .locator("body")
        .evaluate(
            "() => parent.postMessage({ type: 'anchor:ready' }, '*')",
            Value::Null,
        )
        .await?;
    page.get_by_role(Role::new("button").name("Comment on a place"))
        .click()
        .await?;
    frame
        .locator("body")
        .evaluate(
            "() => parent.postMessage({ type: 'anchor:picked', version: 2, kind: 'element', path: 'main > p', nodeId: 'target', quote: 'anchor target', x: 0.2, y: 0.3, approx: false }, '*')",
            Value::Null,
        )
        .await?;
    let composer = page.locator("#vanchor-composer");
    expect(&composer).to_be_visible().await?;
    composer
        .get_by_label("Anchored feedback")
        .fill("Please revise the target copy.")
        .await?;
    expect(&composer.get_by_role(Role::new("button").name("Copy prompt")))
        .to_be_visible()
        .await?;
    for target in [
        composer.get_by_role(Role::new("button").name("Copy prompt")),
        composer.get_by_role(Role::new("button").name("Add comment")),
        composer.get_by_role(Role::new("button").name("Cancel anchored comment")),
    ] {
        let bounds = target.bounding_box().await?;
        let bounds = bounds.unwrap_or_else(|| panic!("{} has no box", target.describe()));
        assert!(bounds.width >= 44.0, "{}: {bounds:?}", target.describe());
        assert!(bounds.height >= 44.0, "{}: {bounds:?}", target.describe());
    }
    composer
        .get_by_role(Role::new("button").name("Copy prompt"))
        .click()
        .await?;
    expect(&composer).to_be_visible().await?;
    expect(&composer.get_by_label("Anchored feedback"))
        .to_have_value("Please revise the target copy.")
        .await?;
    let feedback_path = format!("/{id}/feedback");
    let posted_feedback = page.wait_for_request(move |request| {
        request.method == "POST" && request.url.ends_with(&feedback_path)
    });
    composer
        .get_by_role(Role::new("button").name("Add comment"))
        .click()
        .await?;
    let feedback_request = posted_feedback.await?;
    // `toMatchObject`: every expected property is present and equal.
    let anchor = feedback_request
        .post_data_json()
        .map(|body| body["anchor"].clone())
        .unwrap_or(Value::Null);
    let expected = json!({
        "version": 2, "kind": "element", "path": "main > p", "nodeId": "target",
        "quote": "anchor target", "approx": false, "x": 0.2, "y": 0.3,
    });
    for (field, value) in expected.as_object().into_iter().flatten() {
        assert_eq!(&anchor[field], value, "anchor.{field} in {anchor}");
    }
    expect(&composer.get_by_text(regex("Saved feedback", "")))
        .to_be_visible()
        .await?;
    expect(&composer.get_by_role(Role::new("button").name("Saved")))
        .to_be_disabled()
        .await?;
    expect(&composer.get_by_label("Anchored feedback"))
        .to_have_value("Please revise the target copy.")
        .await?;
    page.keyboard().press("Escape").await?;
    expect(&composer).to_be_hidden().await?;
    expect(&page.get_by_role(Role::new("button").name("Comment on a place")))
        .to_be_focused()
        .await?;
    let scroll_width = page
        .evaluate("document.documentElement.scrollWidth")
        .await?;
    assert!(
        scroll_width.as_f64().unwrap_or(f64::MAX) <= 390.0,
        "{scroll_width}"
    );
    assert_eq!(console_errors(&page, false), Vec::<String>::new());
    Ok(())
}

async fn anchored_composer_clamped(t: Ctx) -> Result<(), Error> {
    let key = t.publisher_key().await?;
    let page = t.page().await?;
    let artifact = t
        .publish(
            &key,
            &format!("PW Anchor placement {}", t.org),
            "<!doctype html><main><p>anchor target</p></main>",
            None,
        )
        .await?;
    let id = id_of(&artifact);
    for (width, height) in [(1440, 900), (768, 1024), (390, 844)] {
        page.set_viewport(width, height, 1.0).await?;
        page.goto(&format!("/{id}")).await?;
        let frame = page.frame_locator("#vframe");
        frame
            .locator("body")
            .evaluate(
                "() => parent.postMessage({ type: 'anchor:ready' }, '*')",
                Value::Null,
            )
            .await?;
        page.get_by_role(Role::new("button").name("Comment on a place"))
            .click()
            .await?;
        frame
            .locator("body")
            .evaluate(
                "() => parent.postMessage({ type: 'anchor:picked', version: 2, kind: 'region', path: 'main', nodeId: 'near-edge', quote: 'anchor target', x: 0.94, y: 0.82, w: 0.04, h: 0.05 }, '*')",
                Value::Null,
            )
            .await?;
        let composer = page.locator("#vanchor-composer");
        expect(&composer).to_be_visible().await?;
        let bounds = composer.bounding_box().await?;
        let bounds = bounds.unwrap_or_else(|| panic!("the composer has no box"));
        let (width, height) = (f64::from(width), f64::from(height));
        assert!(bounds.x >= 0.0, "{bounds:?}");
        assert!(bounds.x + bounds.width <= width, "{bounds:?} in {width}");
        assert!(bounds.y + bounds.height <= height, "{bounds:?} in {height}");
        if width <= 760.0 {
            assert!(
                bounds.y + bounds.height > height - 6.0,
                "{bounds:?} in {height}"
            );
        }
        page.keyboard().press("Escape").await?;
    }
    assert_eq!(console_errors(&page, false), Vec::<String>::new());
    Ok(())
}

async fn raw_csp_sandbox(t: Ctx) -> Result<(), Error> {
    let key = t.publisher_key().await?;
    let artifact = t
        .publish(
            &key,
            &format!("PW Raw {}", t.org),
            "<!doctype html><h1>raw</h1>",
            None,
        )
        .await?;
    let response = t
        .request()
        .get(&format!("/raw/{}", id_of(&artifact)))
        .await?;
    assert_eq!(response.status, 200);
    let csp = response.header("content-security-policy").unwrap_or("");
    assert!(csp.contains("sandbox"), "{csp}");
    assert!(!csp.contains("allow-same-origin"), "{csp}");
    Ok(())
}

async fn download_attachment(t: Ctx) -> Result<(), Error> {
    let key = t.publisher_key().await?;
    let artifact = t
        .publish(
            &key,
            &format!("PW Dl {}", t.org),
            "<!doctype html><h1>dl</h1>",
            None,
        )
        .await?;
    let response = t
        .request()
        .get(&format!("/raw/{}?download=1", id_of(&artifact)))
        .await?;
    assert_eq!(response.status, 200);
    Ok(())
}

async fn history_after_update(t: Ctx) -> Result<(), Error> {
    let key = t.publisher_key().await?;
    let artifact = t
        .publish(
            &key,
            &format!("PW Hist {}", t.org),
            "<!doctype html><h1>v1</h1>",
            None,
        )
        .await?;
    let id = id_of(&artifact);
    let updated = t
        .mcp_call(
            &key,
            "update_artifact",
            json!({"id": id, "html": "<!doctype html><h1>v2</h1>"}),
        )
        .await?;
    assert!(updated.ok(), "{}", updated.status);
    let history = t.request().get(&format!("/{id}/history")).await?;
    assert_eq!(history.status, 200);
    Ok(())
}

async fn discussion_details(t: Ctx) -> Result<(), Error> {
    let key = t.publisher_key().await?;
    let page = t.page().await?;
    let artifact = t
        .publish(
            &key,
            &format!("PW Discussion {}", t.org),
            "<!doctype html><h1>discussion</h1>",
            None,
        )
        .await?;
    page.goto(&format!("/{}", id_of(&artifact))).await?;
    page.locator("#vtitle-toggle").click().await?;
    page.get_by_role(Role::new("menuitem").name("Details"))
        .click()
        .await?;
    expect(&page.get_by_role(Role::new("link").name(regex("Open Thread", "i"))))
        .to_have_count(0)
        .await?;

    expect(&page.locator("#vdiscussion-state"))
        .to_have_text("Artifact MCP only")
        .await?;
    expect(&page.get_by_role(Role::new("button").name("Keep discussion in Artifact MCP")))
        .to_be_visible()
        .await?;
    expect(&page.get_by_role(Role::new("button").name("Enable two-way Discord sync")))
        .to_be_visible()
        .await?;
    expect(&page.get_by_text(regex("Artifact MCP remains canonical", "i")))
        .to_be_visible()
        .await?;

    page.get_by_role(Role::new("button").name("Keep discussion in Artifact MCP"))
        .click()
        .await?;
    expect(&page.locator("#vdiscussion-state"))
        .to_have_text("Artifact MCP only")
        .await?;
    expect(&page.get_by_role(Role::new("button").name("Use organization default")))
        .to_be_focused()
        .await
}

async fn discussion_owner_only(t: Ctx) -> Result<(), Error> {
    let org = &t.org;
    let owner = format!("{org}-discussion-owner@example.test");
    let member = format!("{org}-discussion-member@example.test");
    for email in [&owner, &member] {
        let added = t
            .api(
                "POST",
                &format!("/settings/orgs/{}/emails", encode_component(org)),
                Some(json!({"email": email})),
            )
            .await?;
        assert_eq!(added.status, 200, "{}", added.text());
    }
    let key = t
        .api(
            "POST",
            "/settings/keys",
            Some(json!({
                "clientId": format!("pw-discussion-owner-{}", now_millis()),
                "org": org,
                "label": "discussion owner",
                "ownerEmail": owner,
            })),
        )
        .await?;
    assert_eq!(key.status, 200, "{}", key.text());
    let secret = key.json()["secret"].as_str().unwrap_or("").to_owned();
    let artifact = t
        .publish(
            &secret,
            &format!("PW Discussion owner {org}"),
            "<!doctype html><h1>owner</h1>",
            None,
        )
        .await?;
    let id = id_of(&artifact);
    let owner_page = t.page_as(Identity::email(&owner)).await?;
    owner_page.goto(&format!("{}/{id}", t.base_url())).await?;
    owner_page.locator("#vtitle-toggle").click().await?;
    owner_page
        .get_by_role(Role::new("menuitem").name("Details"))
        .click()
        .await?;
    expect(&owner_page.locator("#vdiscussion-actions"))
        .to_be_attached()
        .await?;
    owner_page.close().await;

    let page = t.page_as(Identity::email(&member)).await?;
    page.goto(&format!("{}/{id}", t.base_url())).await?;
    page.locator("#vtitle-toggle").click().await?;
    page.get_by_role(Role::new("menuitem").name("Details"))
        .click()
        .await?;
    expect(&page.locator("#vdiscussion"))
        .to_be_visible()
        .await?;
    expect(&page.locator("#vdiscussion-actions button"))
        .to_have_count(0)
        .await
}
