// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
//! `artifact action popovers`, from `playwright/tests/12-artifact-popovers.spec.mjs`: the More
//! menu of a canonical card and of its reel and ribbon clones stays inside a phone viewport and
//! closes with Escape.

use browser_tests::e2e::{Case, Ctx, encode_component};
use browser_tests::{Error, Locator, Page, e2e_case, expect};
use serde_json::{Value, json};

const SPEC: &str = "12-artifact-popovers";

/// `page.setViewportSize({ width: 390, height: 520 })`.
const VIEWPORT: (u32, u32) = (390, 520);

pub fn cases() -> Vec<Case> {
    vec![
        e2e_case!(
            SPEC,
            "keeps the main artifact action menu inside the viewport and closes with Escape",
            main_menu
        ),
        e2e_case!(
            SPEC,
            "keeps the reel clone action menu inside the viewport",
            reel_clone_menu
        ),
        e2e_case!(
            SPEC,
            "keeps the ribbons clone action menu inside the viewport",
            ribbons_clone_menu
        ),
    ]
}

fn id_of(value: &Value) -> String {
    value["id"].as_str().unwrap_or("").to_owned()
}

/// `assertInsideViewport`: the menu's box lies inside the viewport and the menu scrolls.
async fn assert_inside_viewport(menu: &Locator<'_>) -> Result<(), Error> {
    let bounds = menu.bounding_box().await?;
    let (width, height) = (f64::from(VIEWPORT.0), f64::from(VIEWPORT.1));
    let Some(bounds) = bounds else {
        panic!("{} has no bounding box", menu.describe());
    };
    assert!(bounds.x >= 0.0, "{bounds:?}");
    assert!(bounds.y >= 0.0, "{bounds:?}");
    assert!(bounds.x + bounds.width <= width, "{bounds:?}");
    assert!(bounds.y + bounds.height <= height, "{bounds:?}");
    let scrolls = menu
        .evaluate(
            "(node) => node.scrollHeight > node.clientHeight || getComputedStyle(node).overflowY === \"auto\"",
            Value::Null,
        )
        .await?;
    assert_eq!(scrolls, json!(true), "the menu scrolls");
    Ok(())
}

/// `openMenu`: click the card's More button; returns the open menu and the trigger.
async fn open_menu<'p>(card: &Locator<'p>) -> Result<(Locator<'p>, Locator<'p>), Error> {
    let trigger = card.locator("[data-action=\"more\"]");
    trigger.click().await?;
    let menu = card.locator(".card-menu:not([hidden])");
    expect(&menu).to_be_visible().await?;
    Ok((menu, trigger))
}

/// Check the menu, then close it with Escape and expect focus back on the trigger.
async fn check_and_close(page: &Page, card: &Locator<'_>) -> Result<(), Error> {
    let (menu, trigger) = open_menu(card).await?;
    assert_inside_viewport(&menu).await?;
    expect(&menu.locator("option[value=\"__create_category__\"]"))
        .to_have_count(1)
        .await?;
    page.keyboard().press("Escape").await?;
    expect(&menu).to_be_hidden().await?;
    expect(&trigger).to_be_focused().await
}

async fn main_menu(t: Ctx) -> Result<(), Error> {
    let key = t.publisher_key().await?;
    let page = t.page().await?;
    let artifact = t
        .publish(
            &key,
            &format!("Popover main {}", t.org),
            "<!doctype html><h1>Popover</h1>",
            None,
        )
        .await?;
    page.set_viewport(VIEWPORT.0, VIEWPORT.1, 1.0).await?;
    page.goto(&format!(
        "/?libraryView=all&org={}",
        encode_component(&t.org)
    ))
    .await?;
    let card = page.locator(&format!(
        "#artifact-grid .card[data-id=\"{}\"]",
        id_of(&artifact)
    ));
    check_and_close(&page, &card).await
}

async fn reel_clone_menu(t: Ctx) -> Result<(), Error> {
    clone_menu(t, "reel").await
}

async fn ribbons_clone_menu(t: Ctx) -> Result<(), Error> {
    clone_menu(t, "ribbons").await
}

/// `keeps the ${view} clone action menu inside the viewport`.
async fn clone_menu(t: Ctx, view: &str) -> Result<(), Error> {
    let key = t.publisher_key().await?;
    let page = t.page().await?;
    let artifact = t
        .publish(
            &key,
            &format!("Popover {view} {}", t.org),
            "<!doctype html><h1>Popover</h1>",
            None,
        )
        .await?;
    let created = t
        .api(
            "POST",
            "/collections",
            Some(json!({
                "org": t.org,
                "name": format!("Popover {view} folder"),
                "artifactIds": [id_of(&artifact)],
            })),
        )
        .await?;
    assert!(created.ok(), "{} {}", created.status, created.text());
    page.set_viewport(VIEWPORT.0, VIEWPORT.1, 1.0).await?;
    page.goto(&format!(
        "/?libraryView={view}&org={}",
        encode_component(&t.org)
    ))
    .await?;
    if view == "reel" {
        page.locator("[data-collection-peek]")
            .filter_has_text(format!("Popover {view} folder"))
            .hover()
            .await?;
    }
    let card = page
        .locator(".collection-reel-card .card, .collection-ribbon-card .card")
        .filter_has_text(format!("Popover {view} {}", t.org))
        .first();
    check_and_close(&page, &card).await
}
