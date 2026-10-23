// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
//! Tests of the locator, action and expectation layer on routed pages (no server): role and text
//! engines, actionability, keyboard and mouse input, frames in and out of process, routes,
//! dialogs, popups and network waiters.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use browser_tests::{
    Browser, BrowserOptions, DialogAction, Error, Network, Page, Pattern, Response, Role,
    chrome_from_env, exact, expect, regex,
};
use serde_json::{Value, json};

const ORIGIN: &str = "https://harness.test";
const OTHER: &str = "https://other.test";

/// Failure screenshots of these tests go under the target directory, not `$TMPDIR`.
fn keep_screenshots_in_target() {
    browser_tests::set_failure_dir(
        std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("locator-failures"),
    );
}

async fn launch() -> Option<Browser> {
    keep_screenshots_in_target();
    let chrome = chrome_from_env()?;
    Some(
        Browser::launch(&BrowserOptions::new(chrome).network(Network::LockedDown))
            .await
            .expect("browser launches"),
    )
}

type Pages = Arc<Mutex<Vec<(String, String)>>>;

/// A page whose `https://harness.test/<name>` and `https://other.test/<name>` serve `pages`.
async fn site(browser: &Browser, pages: &[(&str, &str)]) -> (Page, Pages) {
    let page = browser.new_page().await.unwrap();
    let table: Pages = Arc::new(Mutex::new(
        pages
            .iter()
            .map(|(path, html)| ((*path).to_owned(), (*html).to_owned()))
            .collect(),
    ));
    for origin in [ORIGIN, OTHER] {
        let table = Arc::clone(&table);
        page.route(&format!("{origin}/"), move |request| {
            let table = table.lock().unwrap();
            match table.iter().find(|(path, _)| *path == request.path) {
                Some((_, html)) => Response::ok("text/html; charset=utf-8", html.clone()),
                None => Response::error(404, "missing"),
            }
        })
        .await
        .unwrap();
    }
    (page, table)
}

#[tokio::test]
async fn the_locator_engine_parses_and_installs() {
    let Some(browser) = launch().await else {
        return;
    };
    let page = browser.new_page().await.unwrap();
    let source = include_str!("../src/injected.js");
    assert_eq!(
        page.compile_script(source, "injected.js").await.unwrap(),
        None
    );
    page.set_content("<button>Go</button>").await.unwrap();
    assert_eq!(page.get_by_role("button").count().await.unwrap(), 1);
    // The engine is a non-enumerable symbol property, invisible to the page's own globals.
    let visible = page
        .evaluate("Object.keys(window).some((key) => key.includes('browser'))")
        .await
        .unwrap();
    assert_eq!(visible, json!(false));
    browser.close().await;
}

#[tokio::test]
async fn roles_names_and_hidden_elements_follow_playwright() {
    let Some(browser) = launch().await else {
        return;
    };
    let page = browser.new_page().await.unwrap();
    page.set_content(
        r#"<main>
          <button aria-label="More actions for Report">⋯</button>
          <button>Save <span hidden>secret</span>note</button>
          <button style="display:none">Hidden button</button>
          <div aria-hidden="true"><button>Aria hidden</button></div>
          <label for="org">Filter by organization</label><select id="org"><option>all</option></select>
          <input type="search" aria-label="Search artifacts">
          <label>Note <input id="note"></label>
          <a href="/x">Open</a><a href="/y">Open raw</a>
          <h2>Operate the registry</h2>
          <div role="menu" aria-label="Artifact overview"><div role="menuitemcheckbox" aria-checked="true">Mark as needing work</div></div>
          <input type="checkbox" id="c" checked><label for="c">Enable Discord threads for this organization</label>
          <p>Artifact MCP <b>remains</b> canonical.</p>
        </main>"#,
    )
    .await
    .unwrap();
    let count = async |locator: browser_tests::Locator<'_>| locator.count().await.unwrap();
    assert_eq!(
        count(page.get_by_role(Role::new("button").name("more actions"))).await,
        1
    );
    assert_eq!(
        count(page.get_by_role(Role::new("button").exact_name("Save note"))).await,
        1
    );
    assert_eq!(
        count(page.get_by_role(Role::new("button").name("hidden"))).await,
        0
    );
    assert_eq!(
        count(page.get_by_role(Role::new("button").name("hidden").include_hidden())).await,
        2
    );
    assert_eq!(count(page.get_by_role("combobox")).await, 1);
    assert_eq!(count(page.get_by_label("Filter by organization")).await, 1);
    assert_eq!(
        count(page.get_by_role(Role::new("searchbox").name("Search artifacts"))).await,
        1
    );
    assert_eq!(
        count(page.get_by_role(Role::new("textbox").exact_name("Note"))).await,
        1
    );
    assert_eq!(
        count(page.get_by_role(Role::new("link").name("Open"))).await,
        2
    );
    assert_eq!(
        count(page.get_by_role(Role::new("link").exact_name("Open"))).await,
        1
    );
    assert_eq!(
        count(page.get_by_role(Role::new("heading").name(regex("operate the", "i")))).await,
        1
    );
    assert_eq!(
        count(page.get_by_role(Role::new("menu").name("Artifact overview"))).await,
        1
    );
    assert_eq!(
        count(page.get_by_role(Role::new("menuitemcheckbox").checked(true))).await,
        1
    );
    assert_eq!(
        count(page.get_by_role(Role::new("checkbox").name(regex("Enable Discord threads", "i"))))
            .await,
        1
    );
    // getByText returns the smallest element whose own text matches; a regular expression sees
    // the raw text.
    assert_eq!(count(page.get_by_text("artifact mcp remains")).await, 1);
    assert_eq!(
        count(page.get_by_text(regex("remains canonical", "i"))).await,
        1
    );
    assert_eq!(
        count(page.get_by_text(exact("Artifact MCP remains canonical."))).await,
        1
    );
    assert_eq!(
        count(page.get_by_text(exact("artifact mcp remains canonical."))).await,
        0
    );
    assert_eq!(
        page.get_by_role(Role::new("button").name("Save"))
            .accessible_name()
            .await
            .unwrap(),
        "Save note"
    );
    // Strict mode: an action on two matches fails at once.
    let error = page
        .get_by_role(Role::new("link").name("Open"))
        .click()
        .await
        .unwrap_err();
    assert!(matches!(error, Error::Strict(_)), "{error}");
    browser.close().await;
}

#[tokio::test]
async fn actions_wait_for_actionability_and_dispatch_real_input() {
    let Some(browser) = launch().await else {
        return;
    };
    let (page, _) = site(
        &browser,
        &[(
            "/",
            r#"<!doctype html><body style="margin:0">
            <button id="late" disabled onclick="this.textContent = 'clicked ' + event.isTrusted">Late</button>
            <div id="cover" style="position:fixed;inset:0;background:rgba(0,0,0,.1)"></div>
            <div style="height:2000px"></div>
            <button id="far" onclick="this.dataset.clicks = (Number(this.dataset.clicks || 0) + 1)" ondblclick="this.dataset.double = '1'">Far</button>
            <input id="name" value="old"><textarea id="area">x</textarea>
            <select id="pick"><option value="a">Alpha</option><option value="b">Beta</option></select>
            <input type="checkbox" id="box">
            <div id="keys" tabindex="0"></div>
            <script>
              setTimeout(() => document.querySelector('#late').disabled = false, 300);
              setTimeout(() => document.querySelector('#cover').remove(), 500);
              document.querySelector('#keys').addEventListener('keydown', (e) => e.target.textContent += e.key + (e.shiftKey ? '+S' : '') + ' ');
              document.querySelector('#pick').addEventListener('change', (e) => document.body.dataset.picked = e.target.value);
            </script></body>"#,
        )],
    )
    .await;
    page.goto(&format!("{ORIGIN}/")).await.unwrap();
    // Waits for enabled and for the cover to stop intercepting the click.
    page.locator("#late").click().await.unwrap();
    expect(&page.locator("#late"))
        .to_have_text("clicked true")
        .await
        .unwrap();
    // Scrolled into view, then clicked and double-clicked.
    page.locator("#far").click().await.unwrap();
    expect(&page.locator("#far"))
        .to_have_attribute("data-clicks", "1")
        .await
        .unwrap();
    page.locator("#far").dblclick().await.unwrap();
    expect(&page.locator("#far"))
        .to_have_attribute("data-double", "1")
        .await
        .unwrap();
    page.locator("#name").fill("new value").await.unwrap();
    expect(&page.locator("#name"))
        .to_have_value("new value")
        .await
        .unwrap();
    page.locator("#name").fill("").await.unwrap();
    expect(&page.locator("#name"))
        .to_have_value("")
        .await
        .unwrap();
    page.locator("#area").fill("line").await.unwrap();
    expect(&page.locator("#area"))
        .to_have_value("line")
        .await
        .unwrap();
    assert_eq!(
        page.locator("#pick")
            .select_option(&["Beta"])
            .await
            .unwrap(),
        vec!["b"]
    );
    expect(&page.locator("body"))
        .to_have_attribute("data-picked", "b")
        .await
        .unwrap();
    page.locator("#box").check().await.unwrap();
    expect(&page.locator("#box")).to_be_checked().await.unwrap();
    page.locator("#box").uncheck().await.unwrap();
    expect(&page.locator("#box"))
        .not()
        .to_be_checked()
        .await
        .unwrap();
    page.locator("#keys").press("a").await.unwrap();
    page.keyboard().press("Shift+Tab").await.unwrap();
    page.locator("#keys").press("Enter").await.unwrap();
    expect(&page.locator("#keys"))
        .to_have_text("a Shift+S Tab+S Enter")
        .await
        .unwrap();
    // Expectation failures name the locator and include a screenshot.
    let failure = expect(&page.locator("#name"))
        .timeout(Duration::from_millis(300))
        .to_have_value("nope")
        .await
        .unwrap_err();
    let text = failure.to_string();
    assert!(
        text.contains("locator(\"#name\")") && text.contains("Screenshot:"),
        "{text}"
    );
    assert!(
        text.contains("Expected:   \"nope\"") && text.contains("Received:   \"\""),
        "{text}"
    );
    browser.close().await;
}

#[tokio::test]
async fn a_click_that_lands_elsewhere_is_blocked_and_retried() {
    let Some(browser) = launch().await else {
        return;
    };
    let page = browser.new_page().await.unwrap();
    // The target moves away when the mouse arrives, after the hit-target check: the press lands
    // on the decoy. The interceptor blocks it, and the click is retried at the new position.
    page.set_content(
        r#"<body style="margin:0">
        <button id="decoy" style="position:absolute;left:0;top:0;width:200px;height:50px" onclick="document.body.dataset.decoy = 'clicked'">decoy</button>
        <button id="target" style="position:absolute;left:0;top:0;width:200px;height:50px;z-index:1" onclick="document.body.dataset.target = 'clicked'">target</button>
        <script>
          let moved = false;
          document.addEventListener('mousemove', () => { if (!moved) { moved = true; document.querySelector('#target').style.top = '100px'; } }, true);
        </script></body>"#,
    )
    .await
    .unwrap();
    page.locator("#target").click().await.unwrap();
    expect(&page.locator("body"))
        .to_have_attribute("data-target", "clicked")
        .await
        .unwrap();
    expect(&page.locator("body"))
        .not()
        .to_have_attribute_present("data-decoy")
        .await
        .unwrap();
    browser.close().await;
}

#[tokio::test]
async fn frames_in_and_out_of_process_are_reachable() {
    let Some(browser) = launch().await else {
        return;
    };
    let frame = r##"<!doctype html><body>
        <input aria-label="Note"><button onclick="document.body.dataset.saved = document.querySelector('input').value">Save private note</button>
        <a href="#details">Jump to details</a><div id="details" style="margin-top:3000px">Details</div>
        <script>console.error('from the frame'); fetch('/api').then(r => r.text()).then(t => document.body.dataset.api = t);</script></body>"##;
    let (page, table) = site(
        &browser,
        &[
            (
                "/",
                r#"<!doctype html><body style="margin:0">
                <div style="height:50px"></div>
                <iframe id="sandboxed" src="/frame" sandbox="allow-scripts" style="width:400px;height:300px;border:7px solid black"></iframe>
                <iframe id="cross" src="https://other.test/frame" style="width:400px;height:300px"></iframe></body>"#,
            ),
            ("/frame", frame),
            ("/api", "routed"),
        ],
    )
    .await;
    page.set_extra_http_headers(&[("X-Test-Identity", "someone")])
        .await
        .unwrap();
    let identities = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&identities);
    let table_for_route = Arc::clone(&table);
    page.route_glob("**/api", move |route| {
        let seen = Arc::clone(&seen);
        let table = Arc::clone(&table_for_route);
        async move {
            if route.request().method == "OPTIONS" {
                // The extra header makes the opaque-origin frame's fetch preflighted.
                return route
                    .fulfill(
                        Response::new(204, "text/plain", Vec::new())
                            .header("Access-Control-Allow-Origin", "*")
                            .header("Access-Control-Allow-Headers", "*"),
                    )
                    .await;
            }
            if route.request().header("x-test-identity") == Some("someone") {
                seen.fetch_add(1, Ordering::SeqCst);
            }
            let _ = table;
            // The sandboxed frame has an opaque origin, so its fetch is cross-origin.
            route
                .fulfill(
                    Response::ok("text/plain", "from route")
                        .header("Access-Control-Allow-Origin", "*"),
                )
                .await
        }
    })
    .await
    .unwrap();
    page.goto(&format!("{ORIGIN}/")).await.unwrap();
    for id in ["#sandboxed", "#cross"] {
        let frame = page.frame_locator(id);
        frame
            .get_by_role(Role::new("textbox").exact_name("Note"))
            .fill("Diary")
            .await
            .unwrap();
        frame
            .get_by_role(Role::new("button").name("Save private note"))
            .click()
            .await
            .unwrap();
        expect(&frame.locator("body"))
            .to_have_attribute("data-saved", "Diary")
            .await
            .unwrap();
        // Out-of-process frames get the page's routes and extra headers.
        expect(&frame.locator("body"))
            .to_have_attribute("data-api", "from route")
            .await
            .unwrap();
        let bounds = frame
            .locator("input")
            .bounding_box()
            .await
            .unwrap()
            .unwrap();
        assert!(bounds.y >= 57.0, "{bounds:?}");
    }
    assert_eq!(identities.load(Ordering::SeqCst), 2);
    // Frames are listed with their URLs, and same-document navigations update them.
    let frames = page.frames();
    assert_eq!(frames.len(), 3, "{frames:?}");
    let cross = frames
        .iter()
        .find(|frame| frame.url.starts_with(OTHER))
        .unwrap()
        .id
        .clone();
    page.frame_locator("#cross")
        .get_by_role(Role::new("link").name("Jump to details"))
        .click()
        .await
        .unwrap();
    browser_tests::poll(
        "the frame URL has the fragment",
        || async { Ok(page.frame_url(&cross).unwrap_or_default()) },
        |url| url.ends_with("#details"),
    )
    .await
    .unwrap();
    // Console output of every frame is in the page's log.
    let errors = page
        .log()
        .into_iter()
        .filter(|entry| entry.to_string().contains("from the frame"))
        .count();
    assert_eq!(errors, 2);
    let html = page
        .frame_locator("#cross")
        .locator("html")
        .evaluate("(element) => element.outerHTML", Value::Null)
        .await
        .unwrap();
    assert!(html.as_str().unwrap().contains("Save private note"));
    browser.close().await;
}

#[tokio::test]
async fn navigation_network_dialogs_and_popups() {
    let Some(browser) = launch().await else {
        return;
    };
    let (mut page, _) = site(
        &browser,
        &[
            (
                "/",
                r#"<!doctype html><title>Home</title><body>
                <button id="post" onclick="fetch('/save', {method:'POST', headers:{'x-artifact-mutation':'1','content-type':'application/json'}, body: JSON.stringify({a: 1})})">Post</button>
                <button id="ask" onclick="document.body.dataset.answer = String(confirm('Sure?'))">Ask</button>
                <button id="open" onclick="window.open('').document.write('<title>Popup</title><p>popup page</p>')">Open</button>
                <a id="next" href="/next">Next</a></body>"#,
            ),
            ("/next", "<!doctype html><title>Next</title><p>next page</p>"),
            ("/save", "saved"),
        ],
    )
    .await;
    page.set_base_url(ORIGIN);
    page.enable_browser_log().await.unwrap();
    let response = page.goto("/").await.unwrap().unwrap();
    assert_eq!(response.status, 200);
    assert!(
        page.response_text(&response)
            .await
            .unwrap()
            .contains("<title>Home</title>")
    );
    let request = page.wait_for_request(|request| request.url.ends_with("/save"));
    let response = page.wait_for_response(|response| response.url.ends_with("/save"));
    page.locator("#post").click().await.unwrap();
    let request = request.await.unwrap();
    assert_eq!(request.method, "POST");
    assert_eq!(request.header("x-artifact-mutation"), Some("1"));
    assert_eq!(request.post_data_json(), Some(json!({"a": 1})));
    let history = page.requests();
    assert!(
        history
            .iter()
            .any(|entry| entry.url.ends_with("/save") && entry.method == "POST")
    );
    assert!(history[0].is_navigation, "{:?}", history[0]);
    assert_eq!(response.await.unwrap().status, 200);
    // Browser messages are console entries, as Playwright's `console` event reports them.
    let mark = page.log_mark();
    page.evaluate("fetch('/missing').then(() => {})")
        .await
        .unwrap();
    browser_tests::poll(
        "a resource error is logged",
        || async { Ok(page.log_since(mark)) },
        |log| {
            log.iter().any(|entry| {
                matches!(entry, browser_tests::LogEntry::Console { level, text, .. }
                    if level == "error" && text.contains("status of 404"))
            })
        },
    )
    .await
    .unwrap();
    // Dialogs: accepted by the handler, listed, and not logged as failures.
    page.on_dialog(|dialog| {
        assert_eq!(dialog.kind, "confirm");
        DialogAction::Accept(None)
    });
    page.locator("#ask").click().await.unwrap();
    expect(&page.locator("body"))
        .to_have_attribute("data-answer", "true")
        .await
        .unwrap();
    assert_eq!(page.dialogs().len(), 1);
    assert!(
        page.log_since(mark)
            .iter()
            .all(|entry| !entry.is_failure() || entry.to_string().contains("status of 404"))
    );
    // Popups.
    let popup = page.wait_for_popup();
    page.locator("#open").click().await.unwrap();
    let popup = popup.await.unwrap();
    expect(&popup.get_by_text("popup page"))
        .to_be_visible()
        .await
        .unwrap();
    // History and reload.
    page.locator("#next").click().await.unwrap();
    expect(&page.get_by_text("next page"))
        .to_be_visible()
        .await
        .unwrap();
    page.go_back().await.unwrap();
    expect(&page.locator("#post"))
        .to_be_visible()
        .await
        .unwrap();
    assert!(page.url().ends_with("harness.test/"), "{}", page.url());
    page.reload().await.unwrap();
    expect(&page.locator("#post"))
        .to_be_visible()
        .await
        .unwrap();
    page.set_default_timeout(Duration::from_secs(5));
    browser.close().await;
}

/// A loopback HTTP server: `/` is a page, `/data` is `real data`.
async fn loopback_server() -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut head = Vec::new();
                let mut buffer = [0u8; 1024];
                while !head.windows(4).any(|window| window == b"\r\n\r\n") {
                    match stream.read(&mut buffer).await {
                        Ok(0) | Err(_) => return,
                        Ok(read) => head.extend_from_slice(&buffer[..read]),
                    }
                }
                let line = String::from_utf8_lossy(&head);
                let (kind, body) = if line.starts_with("GET /data") {
                    ("text/plain", "real data")
                } else {
                    ("text/html", "<!doctype html><body>home</body>")
                };
                let reply = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(reply.as_bytes()).await;
            });
        }
    });
    format!("http://{address}")
}

#[tokio::test]
async fn routes_fulfil_continue_abort_fall_back_and_fetch() {
    keep_screenshots_in_target();
    let Some(chrome) = chrome_from_env() else {
        return;
    };
    let browser = Browser::launch(&BrowserOptions::new(chrome).network(Network::LoopbackOnly))
        .await
        .unwrap();
    let base = loopback_server().await;
    let page = browser.new_page().await.unwrap();
    page.goto(&format!("{base}/")).await.unwrap();
    let fetch = async |path: &str| -> Value {
        page.call(
            "async (path) => { try { const r = await fetch(path); return [r.status, await r.text()]; } catch (e) { return ['failed', String(e)]; } }",
            &[json!(path)],
        )
        .await
        .unwrap()
    };
    assert_eq!(fetch("/data").await, json!([200, "real data"]));
    // `fetch` gets the real response, which the handler changes.
    page.route_glob("**/data", |route| async move {
        let mut route = route;
        let upstream = route.fetch().await?;
        let changed = upstream.text().replace("real", "changed");
        route.fulfill(Response::ok("text/plain", changed)).await
    })
    .await
    .unwrap();
    assert_eq!(fetch("/data").await, json!([200, "changed data"]));
    // The newest route runs first; `fallback` passes to the older one.
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&calls);
    page.route_glob("**/data", move |route| {
        let counter = Arc::clone(&counter);
        async move {
            if counter.fetch_add(1, Ordering::SeqCst) == 0 {
                route
                    .fulfill(Response::json(503, &json!({"message": "blocked"})))
                    .await
            } else {
                route.fallback();
                Ok(())
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(
        fetch("/data").await,
        json!([503, "{\"message\":\"blocked\"}"])
    );
    assert_eq!(fetch("/data").await, json!([200, "changed data"]));
    page.unroute("**/data").await.unwrap();
    assert_eq!(fetch("/data").await, json!([200, "real data"]));
    page.route_where(
        |url| url.ends_with("/data"),
        |route| async move { route.abort("Failed").await },
    )
    .await
    .unwrap();
    assert_eq!(fetch("/data").await[0], json!("failed"));
    // `continue_request` lets it through.
    page.route_glob(
        "**/data",
        |route| async move { route.continue_request().await },
    )
    .await
    .unwrap();
    assert_eq!(fetch("/data").await, json!([200, "real data"]));
    page.unroute_all().await.unwrap();
    assert_eq!(fetch("/data").await, json!([200, "real data"]));
    // Off-host stays blocked under LoopbackOnly.
    assert_eq!(fetch("https://example.com/").await[0], json!("failed"));
    browser.close().await;
}

#[tokio::test]
async fn expectations_cover_playwright_matchers() {
    let Some(browser) = launch().await else {
        return;
    };
    let page = browser.new_page().await.unwrap();
    page.set_content(
        r#"<select id="s"><option>All categories</option><option>  Destination   only (0) </option><option>+ Category</option></select>
        <div id="status" class="inline-status error">Needs organization Discord credential and outbound threading policy ready</div>
        <div id="inspector" inert>panel</div><button id="b" disabled>Saved</button><input id="f">
        <div id="later"></div><script>setTimeout(() => { document.querySelector('#later').textContent = 'arrived'; document.querySelector('#later').className = 'show'; }, 400);</script>"#,
    )
    .await
    .unwrap();
    expect(&page.locator("#s option"))
        .to_have_texts(&["All categories", "Destination only (0)", "+ Category"])
        .await
        .unwrap();
    expect(&page.locator("#s option"))
        .to_have_count(3)
        .await
        .unwrap();
    expect(&page.locator("#status"))
        .to_have_class(regex("error", ""))
        .await
        .unwrap();
    expect(&page.locator("#status"))
        .to_contain_text(regex(
            "organization Discord credential.*outbound threading policy.*ready",
            "i",
        ))
        .await
        .unwrap();
    expect(&page.locator("#inspector"))
        .to_have_attribute("inert", "")
        .await
        .unwrap();
    expect(&page.locator("#inspector"))
        .not()
        .to_have_attribute("data-x", regex(".+", ""))
        .await
        .unwrap();
    expect(&page.locator("#b")).to_be_disabled().await.unwrap();
    expect(&page.locator("#nothing"))
        .to_be_hidden()
        .await
        .unwrap();
    expect(&page.locator("#nothing"))
        .to_have_count(0)
        .await
        .unwrap();
    expect(&page.locator("#later"))
        .to_have_text("arrived")
        .await
        .unwrap();
    expect(&page.locator("#later"))
        .to_have_class("show")
        .await
        .unwrap();
    page.locator("#f").focus().await.unwrap();
    expect(&page.locator("#f")).to_be_focused().await.unwrap();
    expect(&page.locator("#f"))
        .to_be_in_viewport()
        .await
        .unwrap();
    expect(&page.locator("#b"))
        .to_have_css("cursor", "default")
        .await
        .unwrap();
    let two = expect(&page.locator("option"))
        .to_be_visible()
        .await
        .unwrap_err();
    assert!(matches!(two, Error::Strict(_)), "{two}");
    let missing: Pattern = "x".into();
    assert!(matches!(missing, Pattern::Substring(_)));
    browser.close().await;
}
