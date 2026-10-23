// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
//! `collection organization`, from `playwright/tests/10-collections.spec.mjs`: folder API
//! memberships, the shared library toolbar, saved preferences, the Reel Shelf, ribbons, contact
//! sheets and the folder view.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use browser_tests::e2e::{Case, Ctx, encode_component};
use browser_tests::{
    Error, Page, Response, Role, WaitState, e2e_case, expect, poll, poll_with_timeout, regex,
};
use serde_json::{Value, json};

const SPEC: &str = "10-collections";

pub fn cases() -> Vec<Case> {
    vec![
        e2e_case!(
            SPEC,
            "creates a folder, adds multiple artifacts atomically, and removes a membership",
            creates_adds_and_removes
        ),
        e2e_case!(
            SPEC,
            "uses the shared toolbar to switch views and preserves the selected organization",
            toolbar_switches_views
        ),
        e2e_case!(
            SPEC,
            "restores the saved layout and reports a preference write failure",
            preference_write_failure
        ),
        e2e_case!(
            SPEC,
            "recovers from an initial folder projection failure with Retry",
            projection_retry
        ),
        e2e_case!(
            SPEC,
            "supports keyboard reel preview, pinning, paging, and action picker",
            keyboard_reel
        ),
        e2e_case!(
            SPEC,
            "repositions the temporary reel around the shelf and dismisses on a second click",
            reel_placement
        ),
        e2e_case!(
            SPEC,
            "collapses and reorders ribbons with persisted preferences",
            ribbons_collapse_and_reorder
        ),
        e2e_case!(
            SPEC,
            "conceals unknown and foreign collection targets",
            conceals_unknown_targets
        ),
        e2e_case!(
            SPEC,
            "creates a folder from the artifact folder picker and keeps the artifact reference",
            picker_creates_folder
        ),
        e2e_case!(
            SPEC,
            "reaches every member through mobile reel paging and preserves an explicit cover",
            mobile_reel_paging
        ),
        e2e_case!(
            SPEC,
            "uses distinct contact sheet compositions for one through four previews",
            sheet_compositions
        ),
        e2e_case!(
            SPEC,
            "dragging an artifact onto a folder adds a reference without copying the artifact",
            drag_adds_reference
        ),
        e2e_case!(
            SPEC,
            "favorite state stays synchronized between the canonical card and reel preview",
            favorite_synchronized
        ),
        e2e_case!(
            SPEC,
            "opens a folder into its focused artifact view and returns to the collection shelf",
            open_folder_and_return
        ),
    ]
}

fn html(title: &str) -> String {
    format!(
        "<!doctype html><meta name=\"viewport\" content=\"width=device-width\"><main><h1>{title}</h1><p>Collection QA artifact.</p></main>"
    )
}

fn id_of(value: &Value) -> String {
    value["id"].as_str().unwrap_or("").to_owned()
}

/// The strings of a JSON array (empty when it is not one).
fn strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect()
}

/// `collections.find((row) => row.id === id)?.artifactIds`.
fn member_ids(projection: &Value, id: &str) -> Option<Vec<String>> {
    projection["collections"]
        .as_array()?
        .iter()
        .find(|row| row["id"] == json!(id))
        .map(|row| strings(&row["artifactIds"]))
}

/// The `beforeEach` hook: reset the run organization's library preferences.
async fn before_each(t: &Ctx) -> Result<(), Error> {
    let org = t.org.clone();
    t.api(
        "PUT",
        &format!("/gallery/preferences?org={}", encode_component(&org)),
        Some(json!({
            "view": "reel",
            "previewSize": "compact",
            "artifactLayout": "grid",
            "collectionOrderByOrg": {org.clone(): []},
            "collapsedCollectionIdsByOrg": {org: []},
        })),
    )
    .await?;
    Ok(())
}

/// `makeArtifacts`: Alpha (UI/UX) and Beta (Research); returns their ids.
async fn make_artifacts(t: &Ctx, key: &str) -> Result<(String, String), Error> {
    let alpha = t
        .publish(key, "Collection QA Alpha", &html("Alpha"), Some("UI/UX"))
        .await?;
    let beta = t
        .publish(key, "Collection QA Beta", &html("Beta"), Some("Research"))
        .await?;
    Ok((id_of(&alpha), id_of(&beta)))
}

/// `makeMany`: `count` artifacts in UI/UX; returns their ids.
async fn make_many(t: &Ctx, key: &str, count: usize) -> Result<Vec<String>, Error> {
    let mut ids = Vec::new();
    for index in 1..=count {
        let artifact = t
            .publish(
                key,
                &format!("Collection QA {index}"),
                &html(&format!("Artifact {index}")),
                Some("UI/UX"),
            )
            .await?;
        ids.push(id_of(&artifact));
    }
    Ok(ids)
}

/// `POST /collections`; returns the response.
async fn create_collection(
    t: &Ctx,
    body: Value,
) -> Result<browser_tests::http::HttpResponse, Error> {
    t.api("POST", "/collections", Some(body)).await
}

/// `getProjection`.
async fn get_projection(t: &Ctx) -> Result<Value, Error> {
    let response = t
        .api(
            "GET",
            &format!(
                "/collections?org={}&include=projection",
                encode_component(&t.org)
            ),
            None,
        )
        .await?;
    assert!(response.ok(), "{} {}", response.status, response.text());
    Ok(response.json())
}

/// The run organization's saved library preferences.
async fn preferences(t: &Ctx) -> Result<Value, Error> {
    Ok(t.api(
        "GET",
        &format!("/gallery/preferences?org={}", encode_component(&t.org)),
        None,
    )
    .await?
    .json())
}

fn library(view: &str, org: &str) -> String {
    format!("/?libraryView={view}&org={}", encode_component(org))
}

/// `selectOrg`: open the filters when the organization filter is folded away, then pick `org`.
async fn select_org(page: &Page, org: &str) -> Result<(), Error> {
    let filter = page.locator("#org-filter");
    if !filter.is_visible().await? {
        page.locator("#library-filters-toggle").click().await?;
    }
    filter.select_option(&[org]).await?;
    Ok(())
}

async fn view_button(page: &Page, view: &str) -> Result<(), Error> {
    page.locator(&format!("[data-library-view='{view}']"))
        .click()
        .await
}

async fn sleep_ms(ms: u64) {
    tokio::time::sleep(Duration::from_millis(ms)).await;
}

/// `new URL(url).searchParams.has(name)`.
fn has_query_param(url: &str, name: &str) -> bool {
    let without_fragment = url.split('#').next().unwrap_or("");
    let Some((_, query)) = without_fragment.split_once('?') else {
        return false;
    };
    query
        .split('&')
        .any(|pair| pair.split('=').next() == Some(name))
}

async fn creates_adds_and_removes(t: Ctx) -> Result<(), Error> {
    before_each(&t).await?;
    let key = t.publisher_key().await?;
    let (alpha, beta) = make_artifacts(&t, &key).await?;
    let created = create_collection(
        &t,
        json!({"org": t.org, "name": "QA collection", "artifactIds": [alpha]}),
    )
    .await?;
    assert_eq!(created.status, 201, "{}", created.text());
    let collection = created.json();
    assert!(
        strings(&collection["artifactIds"]).contains(&alpha),
        "{collection}"
    );
    let collection_id = id_of(&collection);
    let memberships = format!(
        "/collections/{collection_id}/memberships?org={}",
        encode_component(&t.org)
    );
    let add = t
        .api(
            "POST",
            &memberships,
            Some(json!({"artifactIds": [alpha, beta]})),
        )
        .await?;
    assert!(add.ok(), "{} {}", add.status, add.text());
    assert!(
        strings(&add.json()["added"]).contains(&beta),
        "{}",
        add.text()
    );
    let invalid = t
        .api(
            "POST",
            &memberships,
            Some(json!({"artifactIds": [beta, "missing-artifact"]})),
        )
        .await?;
    assert_eq!(invalid.status, 404, "{}", invalid.text());
    let members = member_ids(&get_projection(&t).await?, &collection_id)
        .expect("the collection is in the projection");
    assert!(
        members.contains(&alpha) && members.contains(&beta),
        "{members:?}"
    );
    let remove = t
        .api(
            "DELETE",
            &memberships,
            Some(json!({"artifactIds": [alpha]})),
        )
        .await?;
    assert!(remove.ok(), "{} {}", remove.status, remove.text());
    let members = member_ids(&get_projection(&t).await?, &collection_id)
        .expect("the collection is in the projection");
    assert!(!members.contains(&alpha), "{members:?}");
    Ok(())
}

async fn toolbar_switches_views(t: Ctx) -> Result<(), Error> {
    before_each(&t).await?;
    let key = t.publisher_key().await?;
    let page = t.page().await?;
    let (alpha, _) = make_artifacts(&t, &key).await?;
    let created = create_collection(
        &t,
        json!({"org": t.org, "name": "View QA", "artifactIds": [alpha]}),
    )
    .await?;
    assert!(created.ok(), "{} {}", created.status, created.text());
    page.goto(&library("reel", &t.org)).await?;
    select_org(&page, &t.org).await?;
    expect(&page.locator("#organization-label"))
        .to_have_text(t.org.as_str())
        .await?;
    for (view, surface) in [
        ("reel", ".collection-grid"),
        ("sheets", ".collection-sheet"),
        ("ribbons", ".collection-ribbons"),
    ] {
        view_button(&page, view).await?;
        expect(&page.locator(&format!("[data-library-view='{view}']")))
            .to_have_attribute("aria-pressed", "true")
            .await?;
        expect(&page.locator(surface)).to_be_visible().await?;
    }
    poll(
        "the saved library view",
        || async { Ok(preferences(&t).await?["view"].clone()) },
        |view| *view == "ribbons",
    )
    .await?;
    Ok(())
}

async fn preference_write_failure(t: Ctx) -> Result<(), Error> {
    before_each(&t).await?;
    let key = t.publisher_key().await?;
    let page = t.page().await?;
    let (alpha, _) = make_artifacts(&t, &key).await?;
    let created = create_collection(
        &t,
        json!({"org": t.org, "name": "Preference recovery QA", "artifactIds": [alpha]}),
    )
    .await?;
    assert!(created.ok(), "{} {}", created.status, created.text());
    page.goto(&library("reel", &t.org)).await?;
    select_org(&page, &t.org).await?;
    let blocked = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&blocked);
    page.route_glob("**/gallery/preferences*", move |route| {
        let flag = Arc::clone(&flag);
        async move {
            if !flag.load(Ordering::SeqCst) && route.request().method == "PUT" {
                flag.store(true, Ordering::SeqCst);
                return route
                    .fulfill(Response::json(
                        503,
                        &json!({"message": "Preference write blocked for QA"}),
                    ))
                    .await;
            }
            route.continue_request().await
        }
    })
    .await?;
    view_button(&page, "ribbons").await?;
    expect(&page.locator("[data-library-view='reel']"))
        .to_have_attribute("aria-pressed", "true")
        .await?;
    expect(&page.locator(".collection-status"))
        .to_contain_text("Preference write blocked for QA")
        .await?;
    page.unroute("**/gallery/preferences*").await?;
    view_button(&page, "ribbons").await?;
    poll(
        "the saved library view",
        || async { Ok(preferences(&t).await?["view"].clone()) },
        |view| *view == "ribbons",
    )
    .await?;
    Ok(())
}

async fn projection_retry(t: Ctx) -> Result<(), Error> {
    before_each(&t).await?;
    let key = t.publisher_key().await?;
    let page = t.page().await?;
    let (alpha, _) = make_artifacts(&t, &key).await?;
    let created = create_collection(
        &t,
        json!({"org": t.org, "name": "Retry loading QA", "artifactIds": [alpha]}),
    )
    .await?;
    assert!(created.ok(), "{} {}", created.status, created.text());
    let failed = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&failed);
    page.route_glob("**/collections*include=projection*", move |route| {
        let flag = Arc::clone(&flag);
        async move {
            if !flag.load(Ordering::SeqCst) && route.request().method == "GET" {
                flag.store(true, Ordering::SeqCst);
                return route
                    .fulfill(Response::json(
                        503,
                        &json!({"message": "Projection temporarily unavailable"}),
                    ))
                    .await;
            }
            route.continue_request().await
        }
    })
    .await?;
    page.goto(&library("reel", &t.org)).await?;
    expect(&page.locator(".collection-retry"))
        .to_be_visible()
        .await?;
    expect(&page.locator(".collection-status"))
        .to_contain_text("Projection temporarily unavailable")
        .await?;
    page.locator(".collection-retry").click().await?;
    expect(
        &page
            .locator("[data-collection-peek]")
            .filter_has_text("Retry loading QA"),
    )
    .to_be_visible()
    .await?;
    assert!(failed.load(Ordering::SeqCst), "the projection failed once");
    Ok(())
}

async fn keyboard_reel(t: Ctx) -> Result<(), Error> {
    before_each(&t).await?;
    let key = t.publisher_key().await?;
    let page = t.page().await?;
    let (alpha, beta) = make_artifacts(&t, &key).await?;
    let created = create_collection(
        &t,
        json!({"org": t.org, "name": "Interaction QA", "artifactIds": [alpha, beta]}),
    )
    .await?;
    assert!(created.ok(), "{} {}", created.status, created.text());
    page.goto(&library("reel", &t.org)).await?;
    select_org(&page, &t.org).await?;
    view_button(&page, "reel").await?;
    let folder = page
        .locator("[data-collection-peek]")
        .filter_has_text("Interaction QA");
    folder.focus().await?;
    folder.press("Enter").await?;
    let reel = page.locator(".collection-reel:not([hidden])");
    expect(&reel).to_be_visible().await?;
    expect(&reel.locator(".collection-reel-card"))
        .to_have_count(2)
        .await?;
    expect(&reel)
        .to_have_attribute("data-placement", "below")
        .await?;
    let pin = reel.locator("[data-reel-pin]");
    if pin.get_attribute("aria-label").await?.as_deref() != Some("Unpin preview reel") {
        pin.click().await?;
    }
    expect(&reel).to_have_class(regex("is-pinned", "")).await?;
    let card = reel.locator(".collection-reel-card").first();
    card.locator("[data-action='more']").click().await?;
    expect(&card.locator("[data-collection-folder-picker]"))
        .to_be_visible()
        .await?;
    card.locator("[data-collection-folder-picker]")
        .click()
        .await?;
    expect(&card.locator("[data-collection-folder-panel]"))
        .to_be_visible()
        .await?;
    page.keyboard().press("Escape").await
}

async fn reel_placement(t: Ctx) -> Result<(), Error> {
    before_each(&t).await?;
    let key = t.publisher_key().await?;
    let page = t.page().await?;
    let (alpha, _) = make_artifacts(&t, &key).await?;
    let created = create_collection(
        &t,
        json!({"org": t.org, "name": "Placement QA", "artifactIds": [alpha]}),
    )
    .await?;
    assert!(created.ok(), "{} {}", created.status, created.text());
    page.set_viewport(1200, 680, 1.0).await?;
    page.goto(&library("reel", &t.org)).await?;
    select_org(&page, &t.org).await?;
    view_button(&page, "reel").await?;
    let folder = page
        .locator("[data-collection-peek]")
        .filter_has_text("Placement QA");
    folder.hover().await?;
    let reel = page.locator(".collection-reel:not([hidden])");
    expect(&reel).to_be_visible().await?;
    expect(&reel)
        .to_have_attribute("data-placement", regex("above|below", ""))
        .await?;
    page.evaluate(
        "window.scrollTo(0, Math.max(0, window.scrollY + document.querySelector(\"[data-collection-peek]\").getBoundingClientRect().top - 220))",
    )
    .await?;
    sleep_ms(150).await;
    folder.hover().await?;
    expect(&reel)
        .to_have_attribute("data-placement", regex("above|below", ""))
        .await?;
    let placement_after_scroll = reel.get_attribute("data-placement").await?;
    let geometry = reel
        .evaluate(
            "(node) => ({ top: node.getBoundingClientRect().top, bottom: node.getBoundingClientRect().bottom })",
            Value::Null,
        )
        .await?;
    let top = geometry["top"].as_f64().unwrap_or(f64::NAN);
    let bottom = geometry["bottom"].as_f64().unwrap_or(f64::NAN);
    assert!(bottom > top, "{geometry}");
    if placement_after_scroll.as_deref() == Some("below") {
        assert!(top >= 0.0, "{geometry}");
    }
    page.evaluate("window.scrollTo(0, 0)").await?;
    sleep_ms(100).await;
    folder.click().await?;
    folder.click().await?;
    expect(&page.locator(".collection-reel:not([hidden])"))
        .to_have_count(0)
        .await
}

async fn ribbons_collapse_and_reorder(t: Ctx) -> Result<(), Error> {
    before_each(&t).await?;
    let key = t.publisher_key().await?;
    let page = t.page().await?;
    let (alpha, _) = make_artifacts(&t, &key).await?;
    for name in ["Ribbon A", "Ribbon B"] {
        let created = create_collection(
            &t,
            json!({"org": t.org, "name": name, "artifactIds": [alpha]}),
        )
        .await?;
        assert!(created.ok(), "{} {}", created.status, created.text());
    }
    page.goto(&library("reel", &t.org)).await?;
    select_org(&page, &t.org).await?;
    view_button(&page, "ribbons").await?;
    let before_order = strings(
        &page
            .locator("[data-ribbon-id]")
            .evaluate_all(
                "(nodes) => nodes.map((node) => node.dataset.ribbonId)",
                Value::Null,
            )
            .await?,
    );
    let first_before = before_order.first().cloned().unwrap_or_default();
    let ribbon = page.locator("[data-ribbon-id]").first();
    let id = ribbon
        .get_attribute("data-ribbon-id")
        .await?
        .unwrap_or_default();
    let collapse = ribbon.locator("[data-ribbon-collapse]");
    collapse.focus().await?;
    collapse.press("Enter").await?;
    expect(&collapse)
        .to_have_attribute("aria-expanded", "false")
        .await?;
    expect(&ribbon.locator(".collection-ribbon-fan"))
        .to_be_visible()
        .await?;
    ribbon
        .locator("[data-ribbon-drag]")
        .press("ArrowDown")
        .await?;
    poll(
        "the first ribbon id",
        || async {
            page.locator("[data-ribbon-id]")
                .first()
                .get_attribute("data-ribbon-id")
                .await
        },
        |first| first.as_deref() != Some(first_before.as_str()),
    )
    .await?;
    sleep_ms(300).await;
    page.reload().await?;
    select_org(&page, &t.org).await?;
    view_button(&page, "ribbons").await?;
    expect(&page.locator("[data-ribbon-id]").first())
        .not()
        .to_have_attribute("data-ribbon-id", first_before.as_str())
        .await?;
    expect(&page.locator(&format!("[data-ribbon-id='{id}'] [data-ribbon-collapse]")))
        .to_have_attribute("aria-expanded", "false")
        .await?;
    let org = t.org.clone();
    poll(
        "the collapsed ribbon is saved",
        || async {
            let body = preferences(&t).await?;
            let saved = if body["preferences"].is_null() {
                &body
            } else {
                &body["preferences"]
            };
            Ok(strings(&saved["collapsedCollectionIdsByOrg"][&org]).contains(&id))
        },
        |saved| *saved,
    )
    .await?;
    Ok(())
}

async fn conceals_unknown_targets(t: Ctx) -> Result<(), Error> {
    before_each(&t).await?;
    let key = t.publisher_key().await?;
    let (alpha, _) = make_artifacts(&t, &key).await?;
    let created = create_collection(
        &t,
        json!({"org": t.org, "name": "Concealment QA", "artifactIds": [alpha]}),
    )
    .await?;
    let collection = created.json();
    let unknown = t
        .api(
            "GET",
            "/collections?org=unknown-tenant&include=projection",
            None,
        )
        .await?;
    assert_eq!(unknown.status, 404, "{}", unknown.text());
    let foreign = t
        .api(
            "GET",
            &format!("/collections/{}?org=unknown-tenant", id_of(&collection)),
            None,
        )
        .await?;
    assert_eq!(foreign.status, 404, "{}", foreign.text());
    Ok(())
}

async fn picker_creates_folder(t: Ctx) -> Result<(), Error> {
    before_each(&t).await?;
    let key = t.publisher_key().await?;
    let page = t.page().await?;
    let (alpha, _) = make_artifacts(&t, &key).await?;
    let base = create_collection(
        &t,
        json!({"org": t.org, "name": "Picker source", "artifactIds": [alpha]}),
    )
    .await?;
    assert!(base.ok(), "{} {}", base.status, base.text());
    page.goto(&library("reel", &t.org)).await?;
    select_org(&page, &t.org).await?;
    view_button(&page, "reel").await?;
    page.locator("[data-collection-peek]")
        .filter_has_text("Picker source")
        .hover()
        .await?;
    let card = page.locator(".collection-reel-card .card").first();
    card.locator("[data-action='more']").click().await?;
    card.locator("[data-collection-folder-picker]")
        .click()
        .await?;
    page.locator("[data-picker-new]").click().await?;
    let dialog = page.locator(".collection-create-dialog");
    dialog
        .locator("input[name=name]")
        .fill("Created from picker")
        .await?;
    dialog
        .get_by_role(Role::new("button").name("Create folder"))
        .click()
        .await?;
    sleep_ms(300).await;
    page.evaluate("window.ArtifactCollections?.refresh?.()")
        .await?;
    poll_with_timeout(
        "a folder named Created from picker holds the artifact",
        Duration::from_millis(15_000),
        || async {
            let projection = get_projection(&t).await?;
            Ok(projection["collections"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|row| {
                    row["name"] == "Created from picker"
                        && strings(&row["artifactIds"]).contains(&alpha)
                }))
        },
        |found| *found,
    )
    .await?;
    Ok(())
}

async fn mobile_reel_paging(t: Ctx) -> Result<(), Error> {
    before_each(&t).await?;
    let key = t.publisher_key().await?;
    let page = t.page().await?;
    let artifacts = make_many(&t, &key, 5).await?;
    let created = create_collection(
        &t,
        json!({
            "org": t.org,
            "name": "Mobile reel QA",
            "artifactIds": artifacts,
            "coverArtifactId": artifacts[3],
        }),
    )
    .await?;
    assert!(created.ok(), "{} {}", created.status, created.text());
    let body = created.json();
    assert_eq!(body["coverArtifactId"], json!(artifacts[3]), "{body}");
    page.set_viewport(390, 760, 1.0).await?;
    page.goto(&library("reel", &t.org)).await?;
    select_org(&page, &t.org).await?;
    view_button(&page, "reel").await?;
    page.locator("[data-collection-peek]")
        .filter_has_text("Mobile reel QA")
        .focus()
        .await?;
    let folder = page
        .locator("[data-collection-peek]")
        .filter_has_text("Mobile reel QA");
    folder.press("Enter").await?;
    let reel = page.locator(".collection-reel:not([hidden])");
    let mut seen = BTreeSet::new();
    poll(
        "the first reel image has loaded",
        || async {
            reel.locator("img")
                .first()
                .evaluate(
                    "(node) => node.complete && node.naturalWidth > 0",
                    Value::Null,
                )
                .await
        },
        |loaded| *loaded == json!(true),
    )
    .await?;
    let compact_card = reel
        .locator(".collection-reel-card .card")
        .first()
        .bounding_box()
        .await?;
    assert!(
        compact_card.is_some_and(|card| card.height < 350.0),
        "{compact_card:?}"
    );
    for _ in 0..artifacts.len() {
        let ids = reel
            .locator("[data-collection-artifact]")
            .evaluate_all(
                "(nodes) => nodes.map((node) => node.dataset.collectionArtifact)",
                Value::Null,
            )
            .await?;
        seen.extend(strings(&ids));
        let next = reel.locator("[data-reel-next]");
        // `isDisabled()` waits for the element, then reads its state.
        next.wait_for(WaitState::Attached).await?;
        if !next.is_enabled().await? {
            break;
        }
        next.click().await?;
    }
    let expected: BTreeSet<String> = artifacts.iter().cloned().collect();
    assert_eq!(seen, expected);
    view_button(&page, "sheets").await?;
    let sheet = page
        .locator(".collection-sheet-card")
        .filter_has_text("Mobile reel QA");
    expect(&sheet.locator(".collection-sheet-cover img").first())
        .to_have_attribute("src", regex(artifacts[3].as_str(), ""))
        .await
}

async fn sheet_compositions(t: Ctx) -> Result<(), Error> {
    before_each(&t).await?;
    let key = t.publisher_key().await?;
    let page = t.page().await?;
    let artifacts = make_many(&t, &key, 4).await?;
    for count in 1..=4 {
        let created = create_collection(
            &t,
            json!({
                "org": t.org,
                "name": format!("Composition {count} QA"),
                "artifactIds": artifacts[..count],
            }),
        )
        .await?;
        assert!(created.ok(), "{} {}", created.status, created.text());
    }
    page.goto(&library("sheets", &t.org)).await?;
    select_org(&page, &t.org).await?;
    view_button(&page, "sheets").await?;
    for count in 1..=4 {
        let sheet = page
            .locator(".collection-sheet-card")
            .filter_has_text(format!("Composition {count} QA"));
        let data_count = if count == 4 {
            "more".to_owned()
        } else {
            count.to_string()
        };
        expect(&sheet.locator(&format!(
            ".collection-sheet-cover[data-count='{data_count}']"
        )))
        .to_be_visible()
        .await?;
        expect(&sheet.locator(".sheet-shot"))
            .to_have_count(count)
            .await?;
    }
    Ok(())
}

async fn drag_adds_reference(t: Ctx) -> Result<(), Error> {
    before_each(&t).await?;
    let key = t.publisher_key().await?;
    let page = t.page().await?;
    let (alpha, _) = make_artifacts(&t, &key).await?;
    let source = create_collection(
        &t,
        json!({"org": t.org, "name": "Drag source", "artifactIds": [alpha]}),
    )
    .await?;
    let target = create_collection(&t, json!({"org": t.org, "name": "Drag target"})).await?;
    assert!(
        source.ok() && target.ok(),
        "{} {}",
        source.status,
        target.status
    );
    let source_id = id_of(&source.json());
    let target_id = id_of(&target.json());
    page.goto(&library("reel", &t.org)).await?;
    select_org(&page, &t.org).await?;
    view_button(&page, "reel").await?;
    page.locator("[data-collection-peek]")
        .filter_has_text("Drag source")
        .hover()
        .await?;
    page.call(
        r#"({ artifactId, targetId }) => {
            const card = document.querySelector(`[data-collection-artifact='${artifactId}']`);
            const target = document.querySelector(`[data-collection-id='${targetId}']`);
            const data = new DataTransfer(); data.setData("text/plain", artifactId);
            card.dispatchEvent(new DragEvent("dragstart", { bubbles: true, dataTransfer: data }));
            target.dispatchEvent(new DragEvent("dragover", { bubbles: true, dataTransfer: data }));
            target.dispatchEvent(new DragEvent("drop", { bubbles: true, dataTransfer: data }));
        }"#,
        &[json!({"artifactId": alpha, "targetId": target_id})],
    )
    .await?;
    poll(
        "the target folder holds the artifact",
        || async {
            Ok(member_ids(&get_projection(&t).await?, &target_id)
                .is_some_and(|members| members.contains(&alpha)))
        },
        |added| *added,
    )
    .await?;
    let projection = get_projection(&t).await?;
    let source_members = member_ids(&projection, &source_id).unwrap_or_default();
    assert!(source_members.contains(&alpha), "{source_members:?}");
    Ok(())
}

async fn favorite_synchronized(t: Ctx) -> Result<(), Error> {
    before_each(&t).await?;
    let key = t.publisher_key().await?;
    let page = t.page().await?;
    let (alpha, _) = make_artifacts(&t, &key).await?;
    let created = create_collection(
        &t,
        json!({"org": t.org, "name": "Favorite QA", "artifactIds": [alpha]}),
    )
    .await?;
    assert!(created.ok(), "{} {}", created.status, created.text());
    page.goto(&library("reel", &t.org)).await?;
    select_org(&page, &t.org).await?;
    view_button(&page, "all").await?;
    let canonical = page.locator(&format!("#artifact-grid .card[data-id='{alpha}']"));
    expect(&canonical)
        .to_have_class(regex("preview-ready", ""))
        .await?;
    canonical
        .locator("[data-action='favorite']")
        .click()
        .await?;
    expect(&canonical.locator("[data-action='favorite']"))
        .to_have_attribute("aria-pressed", "true")
        .await?;
    view_button(&page, "reel").await?;
    page.evaluate("window.ArtifactCollections?.refresh?.()")
        .await?;
    page.locator("[data-collection-peek]")
        .filter_has_text("Favorite QA")
        .hover()
        .await?;
    let preview = page.locator(&format!(
        ".collection-reel-card [data-collection-artifact='{alpha}']"
    ));
    expect(&preview)
        .to_have_class(regex("preview-ready", ""))
        .await?;
    let preview_box = preview.bounding_box().await?;
    assert!(
        preview_box.is_some_and(|preview| preview.height < 350.0),
        "{preview_box:?}"
    );
    expect(&preview.locator("[data-action='favorite']"))
        .to_have_attribute("aria-pressed", "true")
        .await
}

async fn open_folder_and_return(t: Ctx) -> Result<(), Error> {
    before_each(&t).await?;
    let key = t.publisher_key().await?;
    let page = t.page().await?;
    let (alpha, _) = make_artifacts(&t, &key).await?;
    let created = create_collection(
        &t,
        json!({"org": t.org, "name": "Open folder QA", "artifactIds": [alpha]}),
    )
    .await?;
    assert!(created.ok(), "{} {}", created.status, created.text());
    page.goto(&library("reel", &t.org)).await?;
    select_org(&page, &t.org).await?;
    view_button(&page, "reel").await?;
    page.locator("[data-collection-peek]")
        .filter_has_text("Open folder QA")
        .hover()
        .await?;
    page.get_by_role(Role::new("button").name("Open folder ↗"))
        .click()
        .await?;
    expect(&page.locator("[data-collection-back]"))
        .to_be_visible()
        .await?;
    expect(&page.locator(".collection-folder-heading h2"))
        .to_have_text("Open folder QA")
        .await?;
    expect(&page.locator(&format!("#artifact-grid .card[data-id='{alpha}']")))
        .to_be_visible()
        .await?;
    page.locator("[data-collection-back]").click().await?;
    expect(&page.locator("[data-library-view='reel']"))
        .to_have_attribute("aria-pressed", "true")
        .await?;
    expect(&page.locator(".collection-grid"))
        .to_be_visible()
        .await?;
    let url = page.url();
    assert!(!has_query_param(&url, "collection"), "{url}");
    Ok(())
}
