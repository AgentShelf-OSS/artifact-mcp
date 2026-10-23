// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
//! Web performance guards, from `playwright/tests/18-web-performance.spec.mjs`: no separate
//! application asset requests, no needless re-sorting or re-rendering, deferred and single
//! fetches, and a late inline shell that still works.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use browser_tests::e2e::{Case, Ctx, encode_component};
use browser_tests::{Error, Response, Role, WaitState, e2e_case, expect, poll};
use serde_json::{Value, json};
use tokio::sync::watch;

const SPEC: &str = "18-web-performance";

pub fn cases() -> Vec<Case> {
    vec![
        e2e_case!(
            SPEC,
            "gallery and viewer start without separate application asset requests",
            no_asset_requests
        ),
        e2e_case!(
            SPEC,
            "search preserves sort and skips inactive collection cards",
            search_preserves_sort
        ),
        e2e_case!(
            SPEC,
            "Details fetches discussion status once on first open",
            discussion_fetched_once
        ),
        e2e_case!(
            SPEC,
            "voice capability states control reader availability",
            voice_capability
        ),
        e2e_case!(
            SPEC,
            "late inline shell still paints existing anchors and accepts early state readiness",
            late_inline_shell
        ),
    ]
}

/// `new URL(url).pathname`.
fn path_of(url: &str) -> &str {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let path = rest.find('/').map_or("/", |slash| &rest[slash..]);
    path.split(['?', '#']).next().unwrap_or(path)
}

fn id_of(row: &Value) -> String {
    row["id"].as_str().unwrap_or("").to_owned()
}

async fn no_asset_requests(t: Ctx) -> Result<(), Error> {
    let key = t.publisher_key().await?;
    let page = t.page().await?;
    let row = t
        .publish(
            &key,
            "Inline application fixture",
            "<!doctype html><main><p>Inline fixture.</p></main>",
            None,
        )
        .await?;
    page.goto("/").await?;
    expect(&page.get_by_role(Role::new("searchbox").name("Search artifacts")))
        .to_be_visible()
        .await?;
    poll(
        "the portal and collections scripts are ready",
        || async {
            page.evaluate("Boolean(window.ArtifactPortal && window.ArtifactCollections)")
                .await
        },
        |ready| *ready == json!(true),
    )
    .await?;
    page.goto(&format!("/{}", id_of(&row))).await?;
    expect(&page.locator("#vtitle-toggle"))
        .to_be_visible()
        .await?;
    expect(&page.frame_locator("#vframe").locator("main"))
        .to_have_text("Inline fixture.")
        .await?;
    // `page.on("request")` from the start: every request of the page and its frames.
    let asset_requests: Vec<String> = page
        .requests()
        .into_iter()
        .filter(|request| path_of(&request.url).starts_with("/assets/"))
        .map(|request| request.url)
        .collect();
    assert_eq!(asset_requests, Vec::<String>::new());
    Ok(())
}

const STATS: &str = "({ sort: window.ArtifactPortal.getPerformanceStats().sortRuns, enhanced: window.ArtifactCollections.getPerformanceStats().canonicalEnhancements })";

async fn search_preserves_sort(t: Ctx) -> Result<(), Error> {
    let key = t.publisher_key().await?;
    let page = t.page().await?;
    let org = t.org.clone();
    let mut rows = Vec::new();
    for title in ["Perf Gamma", "Perf Alpha", "Perf Beta"] {
        rows.push(
            t.publish(
                &key,
                title,
                "<!doctype html><main><p>Performance regression fixture.</p></main>",
                None,
            )
            .await?,
        );
    }
    let ids: Vec<String> = rows.iter().map(id_of).collect();
    let created = t
        .api(
            "POST",
            "/collections",
            Some(json!({"org": org, "name": "Performance folder", "artifactIds": ids})),
        )
        .await?;
    assert!(created.ok(), "{} {}", created.status, created.text());
    page.goto(&format!("/?libraryView=all&org={}", encode_component(&org)))
        .await?;
    poll(
        "the number of collections",
        || async {
            page.evaluate("window.ArtifactCollections?.getState().collections.length")
                .await
        },
        |count| count.as_f64().is_some_and(|count| count > 0.0),
    )
    .await?;
    expect(&page.locator("#collection-surface .collection-artifact-card"))
        .to_have_count(0)
        .await?;
    let before = page.evaluate(STATS).await?;
    let search = page.get_by_role(Role::new("searchbox").name("Search artifacts"));
    let visible_titles = page.locator("#artifact-grid > .card:visible .card-title");
    for title in ["Perf Alpha", "Perf Beta", "Perf Gamma"] {
        search.fill(title).await?;
        expect(&visible_titles).to_have_texts(&[title]).await?;
    }
    search.fill("").await?;
    let after = page.evaluate(STATS).await?;
    assert_eq!(after, before);
    page.get_by_label("Sort artifacts")
        .select_option(&["title"])
        .await?;
    search.fill("Perf ").await?;
    expect(&visible_titles)
        .to_have_texts(&["Perf Alpha", "Perf Beta", "Perf Gamma"])
        .await?;
    page.locator("[data-library-view=\"reel\"]").click().await?;
    expect(&page.locator(".collection-grid"))
        .to_be_visible()
        .await?;
    expect(
        &page
            .locator("[data-collection-peek]")
            .filter_has_text("Performance folder"),
    )
    .to_be_visible()
    .await
}

async fn discussion_fetched_once(t: Ctx) -> Result<(), Error> {
    let key = t.publisher_key().await?;
    let page = t.page().await?;
    let row = t
        .publish(
            &key,
            "Deferred discussion fixture",
            "<!doctype html><main><p>Viewer fixture.</p></main>",
            None,
        )
        .await?;
    let id = id_of(&row);
    page.route_glob(
        &format!("**/{id}/discussion/override"),
        |route| async move {
            route
                .fulfill(Response::json(
                    200,
                    &json!({"state": "local", "overrideMode": "inherit"}),
                ))
                .await
        },
    )
    .await?;
    let override_path = format!("/{id}/discussion/override");
    // `page.on("request")`, counted from the start of the page.
    let requests = || {
        page.requests()
            .into_iter()
            .filter(|request| path_of(&request.url) == override_path)
            .count()
    };
    page.goto(&format!("/{id}")).await?;
    page.frame_locator("#vframe")
        .locator("main")
        .wait_for(WaitState::Visible)
        .await?;
    assert_eq!(requests(), 0);
    page.locator("#vtitle-toggle").click().await?;
    page.locator("[data-inspector-open=\"details\"]")
        .click()
        .await?;
    poll(
        "discussion override requests",
        || async { Ok(requests()) },
        |count| *count == 1,
    )
    .await?;
    expect(&page.locator("#vdiscussion-state"))
        .not()
        .to_have_text("Checking status…")
        .await?;
    page.locator("#vinspector-close").click().await?;
    page.locator("#vtitle-toggle").click().await?;
    page.locator("[data-inspector-open=\"details\"]")
        .click()
        .await?;
    assert_eq!(requests(), 1);
    Ok(())
}

async fn voice_capability(t: Ctx) -> Result<(), Error> {
    let key = t.publisher_key().await?;
    let page = t.page().await?;
    let row = t
        .publish(
            &key,
            "Voice capability fixture",
            "<!doctype html><main><p>Read this fixture.</p></main>",
            None,
        )
        .await?;
    let id = id_of(&row);
    let mode = Arc::new(Mutex::new("disabled"));
    let current = Arc::clone(&mode);
    page.route_glob(&format!("**/{id}/speech/voices"), move |route| {
        let mode = *current.lock().unwrap();
        async move {
            let response = match mode {
                "error" => Response::new(503, "text/plain", "unavailable"),
                "disabled" => Response::json(200, &json!({"enabled": false, "voices": []})),
                _ => Response::json(
                    200,
                    &json!({"enabled": true, "voices": [{"id": "fixture_voice", "name": "Fixture Voice", "provider": "Fixture"}]}),
                ),
            };
            route.fulfill(response).await
        }
    })
    .await?;
    let voices_path = format!("/{id}/speech/voices");
    let voices_response = || {
        let path = voices_path.clone();
        page.wait_for_response(move |response| path_of(&response.url) == path)
    };
    let reader = page.locator(".vreader");
    let waiter = voices_response();
    page.goto(&format!("/{id}")).await?;
    waiter.await?;
    expect(&reader).to_have_count(1).await?;
    expect(&reader).to_be_hidden().await?;
    *mode.lock().unwrap() = "enabled";
    page.reload().await?;
    expect(&page.locator("#vreader-toggle"))
        .to_be_enabled()
        .await?;
    *mode.lock().unwrap() = "error";
    let waiter = voices_response();
    page.reload().await?;
    waiter.await?;
    expect(&reader).to_have_count(1).await?;
    expect(&reader).to_be_hidden().await
}

const LATE_SHELL_HTML: &str = r#"<!doctype html><main><p>anchor target</p><output id="state-ready">Waiting</output></main><script>
    addEventListener('message', event => { if (event.source === parent && event.data?.type === 'state:ready') document.querySelector('#state-ready').textContent = String(event.data.enabled); });
    parent.postMessage({type:'state:hello'}, '*');
  </script>"#;

async fn late_inline_shell(t: Ctx) -> Result<(), Error> {
    let key = t.publisher_key().await?;
    let page = t.page().await?;
    let row = t
        .publish(&key, "Late shell anchor fixture", LATE_SHELL_HTML, None)
        .await?;
    let id = id_of(&row);
    let created = t
        .api(
            "POST",
            &format!("/{id}/feedback"),
            Some(json!({
                "body": "boot marker",
                "anchor": {"version": 2, "kind": "element", "path": "main > p", "nodeId": "boot", "quote": "anchor target", "x": 0.25, "y": 0.3, "approx": false},
            })),
        )
        .await?;
    assert!(created.ok(), "{} {}", created.status, created.text());

    let held = Arc::new(AtomicBool::new(false));
    let (release, released) = watch::channel(false);
    // `expect(html).toContain(...)` inside the route handler: a failure is recorded here and
    // fails the test.
    let route_failures: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let failures = Arc::clone(&route_failures);
    page.route_glob(&format!("**/{id}"), move |mut route| {
        let failures = Arc::clone(&failures);
        async move {
            let response = route.fetch().await?;
            let html = response.text();
            if !html.contains("<div id=\"shell-config\"") {
                failures.lock().unwrap().push(format!(
                    "{} does not contain <div id=\"shell-config\"",
                    route.request().url
                ));
                return route.continue_request().await;
            }
            let body = html.replacen(
                "<div id=\"shell-config\"",
                "<script src=\"/__qa_hold_shell\"></script><div id=\"shell-config\"",
                1,
            );
            // The body changes, so its length and encoding headers no longer apply.
            let headers = response
                .headers
                .into_iter()
                .filter(|(name, _)| {
                    !name.eq_ignore_ascii_case("content-length")
                        && !name.eq_ignore_ascii_case("content-encoding")
                })
                .collect();
            route
                .fulfill(Response {
                    status: response.status,
                    headers,
                    body: body.into_bytes(),
                })
                .await
        }
    })
    .await?;
    let hold = Arc::clone(&held);
    page.route_glob("**/__qa_hold_shell", move |route| {
        let hold = Arc::clone(&hold);
        let mut released = released.clone();
        async move {
            if !hold.swap(true, Ordering::SeqCst) {
                let _ = released.wait_for(|released| *released).await;
            }
            route
                .fulfill(Response::ok("application/javascript", ""))
                .await
        }
    })
    .await?;

    let outcome: Result<(), Error> = async {
        // `page.goto(url, { waitUntil: "commit" })`: the document's load waits for the held
        // script, so only the navigation's commit is awaited.
        let url = format!("{}/{id}", t.base_url());
        let navigation = page
            .command(
                "Page.navigate",
                json!({"url": url, "frameId": page.main_frame_id()}),
            )
            .await?;
        if let Some(error) = navigation.get("errorText").and_then(Value::as_str) {
            return Err(Error::Navigation(format!("{url}: {error}")));
        }
        page.locator("#vframe")
            .wait_for(WaitState::Attached)
            .await?;
        // `rawLoaded`: a frame navigated to the raw document (Playwright's `framenavigated`).
        let raw_path = format!("/raw/{id}");
        let deadline = tokio::time::Instant::now() + page.default_timeout();
        while !page
            .frames()
            .iter()
            .any(|frame| frame.url.contains(&raw_path))
        {
            if tokio::time::Instant::now() >= deadline {
                return Err(Error::Timeout {
                    what: format!("a frame navigated to {raw_path}"),
                    after: page.default_timeout(),
                });
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        poll(
            "the shell script is held",
            || async { Ok(held.load(Ordering::SeqCst)) },
            |held| *held,
        )
        .await?;
        let frame = page.frame_locator("#vframe");
        expect(&frame.locator("#state-ready"))
            .to_have_text("Waiting")
            .await?;
        let _ = release.send(true);
        let anchors = page.locator("#vanchor-overlay [id^=\"vanchor-\"]");
        expect(&anchors).to_be_visible().await?;
        expect(&frame.locator("#state-ready"))
            .to_have_text("true")
            .await?;
        assert_eq!(anchors.count().await?, 1);
        Ok(())
    }
    .await;
    // `finally`
    let _ = release.send(true);
    page.unroute("**/__qa_hold_shell").await?;
    page.unroute(&format!("**/{id}")).await?;
    outcome?;
    let failures = route_failures.lock().unwrap().clone();
    assert_eq!(failures, Vec::<String>::new());
    Ok(())
}
