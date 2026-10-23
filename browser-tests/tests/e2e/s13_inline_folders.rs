// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
//! `inline folder memberships`, from `playwright/tests/13-inline-folders.spec.mjs`: the folder
//! checkboxes inside an artifact's More menu write memberships at once, roll back on failure,
//! work in the reel and ribbon clones, and stay inside a phone viewport.

use browser_tests::e2e::{Case, Ctx, encode_component, now_millis};
use browser_tests::{Error, Locator, Page, Response, Role, e2e_case, expect, poll};
use serde_json::{Value, json};

const SPEC: &str = "13-inline-folders";

pub fn cases() -> Vec<Case> {
    vec![
        e2e_case!(
            SPEC,
            "dismissing More resets the nested folder control",
            dismissing_more_resets
        ),
        e2e_case!(
            SPEC,
            "the artifact popup adds and removes memberships immediately without a modal",
            popup_adds_and_removes
        ),
        e2e_case!(
            SPEC,
            "failed membership changes roll back the checkbox and show a retryable inline error",
            failed_membership_rolls_back
        ),
        e2e_case!(
            SPEC,
            "the reel preview keeps its folder picker usable after membership writes",
            reel_preview_picker
        ),
        e2e_case!(
            SPEC,
            "the ribbons preview keeps its folder picker usable after membership writes",
            ribbons_preview_picker
        ),
        e2e_case!(
            SPEC,
            "new-folder Cancel returns to the expanded artifact popup",
            new_folder_cancel
        ),
        e2e_case!(
            SPEC,
            "new-folder Close returns to the expanded artifact popup",
            new_folder_close
        ),
        e2e_case!(
            SPEC,
            "new-folder Escape returns to the expanded artifact popup",
            new_folder_escape
        ),
        e2e_case!(
            SPEC,
            "expanded folder controls remain within the mobile viewport",
            mobile_viewport
        ),
    ]
}

fn id_of(value: &Value) -> String {
    value["id"].as_str().unwrap_or("").to_owned()
}

/// What `fixture()` returns: the artifact, the empty destination folder, the artifact's card in
/// the library grid and its open folder panel.
struct Opened<'p> {
    artifact_id: String,
    folder_id: String,
    folder_name: String,
    card: Locator<'p>,
    panel: Locator<'p>,
}

/// `fixture()`: publish an artifact, create an empty folder, open the library and expand the
/// artifact's folder control inside its More menu.
async fn fixture<'p>(t: &Ctx, page: &'p Page) -> Result<Opened<'p>, Error> {
    let key = t.publisher_key().await?;
    let artifact = t
        .publish(
            &key,
            &format!("Inline folders {}", now_millis()),
            "<!doctype html><h1>Inline folders</h1>",
            None,
        )
        .await?;
    let created = t
        .api(
            "POST",
            "/collections",
            Some(json!({
                "org": t.org,
                "name": format!(
                    "Inline destination {} with a longer name to verify picker wrapping",
                    now_millis()
                ),
            })),
        )
        .await?;
    assert!(created.ok(), "{} {}", created.status, created.text());
    let folder = created.json();
    page.goto(&format!(
        "/?libraryView=reel&org={}",
        encode_component(&t.org)
    ))
    .await?;
    let artifact_id = id_of(&artifact);
    let card = page.locator(&format!("#artifact-grid .card[data-id=\"{artifact_id}\"]"));
    // Folder controls are added after the initial collection projection loads.
    expect(&card.locator("[data-collection-folder-picker]"))
        .to_be_attached()
        .await?;
    card.locator("[data-action=\"more\"]").click().await?;
    card.locator("[data-collection-folder-picker]")
        .click()
        .await?;
    let panel = card.locator("[data-collection-folder-panel]");
    expect(&panel).to_be_visible().await?;
    Ok(Opened {
        artifact_id,
        folder_id: id_of(&folder),
        folder_name: folder["name"].as_str().unwrap_or("").to_owned(),
        card,
        panel,
    })
}

/// `GET /collections?org=…`: the `artifactIds` of collection `id`, `None` when it is missing.
async fn members(t: &Ctx, id: &str) -> Result<Option<Vec<String>>, Error> {
    let response = t
        .api(
            "GET",
            &format!("/collections?org={}", encode_component(&t.org)),
            None,
        )
        .await?;
    Ok(response.json()["collections"]
        .as_array()
        .and_then(|rows| rows.iter().find(|row| row["id"] == json!(id)))
        .map(|row| {
            row["artifactIds"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        }))
}

/// `expect.poll(() => members(id)).toContain(artifact)` (or `.not.toContain` when `!present`).
async fn poll_membership(t: &Ctx, id: &str, artifact: &str, present: bool) -> Result<(), Error> {
    let what = if present {
        format!("collection {id} contains {artifact}")
    } else {
        format!("collection {id} does not contain {artifact}")
    };
    poll(
        &what,
        || async { members(t, id).await },
        |ids| {
            ids.as_ref()
                .is_some_and(|ids| ids.iter().any(|member| member == artifact) == present)
        },
    )
    .await?;
    Ok(())
}

async fn dismissing_more_resets(t: Ctx) -> Result<(), Error> {
    let page = t.page().await?;
    let Opened { card, panel, .. } = fixture(&t, &page).await?;
    page.keyboard().press("Escape").await?;
    expect(&card.locator(".card-menu")).to_be_hidden().await?;
    expect(&panel).to_be_hidden().await?;
    expect(&card.locator("[data-collection-folder-picker]"))
        .to_have_attribute("aria-expanded", "false")
        .await?;
    card.locator("[data-action=\"more\"]").click().await?;
    expect(&card.locator(".card-menu")).to_be_visible().await?;
    expect(&panel).to_be_hidden().await
}

async fn popup_adds_and_removes(t: Ctx) -> Result<(), Error> {
    let page = t.page().await?;
    let opened = fixture(&t, &page).await?;
    let card = &opened.card;
    let checkbox = card.locator(&format!(
        "[data-collection-folder-option=\"{}\"]",
        opened.folder_id
    ));
    expect(&page.locator(".collection-picker-dialog"))
        .to_have_count(0)
        .await?;
    checkbox.check().await?;
    poll_membership(&t, &opened.folder_id, &opened.artifact_id, true).await?;
    expect(&checkbox).to_be_enabled().await?;
    expect(&checkbox).to_be_checked().await?;
    expect(&card.locator("[data-collection-folder-picker]"))
        .to_contain_text(opened.folder_name.as_str())
        .await?;
    checkbox.uncheck().await?;
    poll_membership(&t, &opened.folder_id, &opened.artifact_id, false).await?;
    expect(&checkbox).not().to_be_checked().await?;
    expect(&card.locator("[data-collection-folder-panel]"))
        .to_be_visible()
        .await
}

async fn failed_membership_rolls_back(t: Ctx) -> Result<(), Error> {
    let page = t.page().await?;
    let opened = fixture(&t, &page).await?;
    let checkbox = opened.card.locator(&format!(
        "[data-collection-folder-option=\"{}\"]",
        opened.folder_id
    ));
    let glob = format!("**/collections/{}/memberships*", opened.folder_id);
    page.route_glob(&glob, |route| async move {
        route
            .fulfill(Response::json(
                503,
                &json!({"message": "Membership unavailable"}),
            ))
            .await
    })
    .await?;
    checkbox.click().await?;
    expect(&opened.panel.locator(".category-error"))
        .to_have_text("Membership unavailable")
        .await?;
    expect(&checkbox).not().to_be_checked().await?;
    expect(&checkbox).to_be_enabled().await?;
    page.unroute(&glob).await?;
    checkbox.check().await?;
    expect(&checkbox).to_be_enabled().await?;
    expect(&checkbox).to_be_checked().await
}

async fn reel_preview_picker(t: Ctx) -> Result<(), Error> {
    preview_picker(t, "reel").await
}

async fn ribbons_preview_picker(t: Ctx) -> Result<(), Error> {
    preview_picker(t, "ribbons").await
}

/// `the ${view} preview keeps its folder picker usable after membership writes`.
async fn preview_picker(t: Ctx, view: &str) -> Result<(), Error> {
    let key = t.publisher_key().await?;
    let page = t.page().await?;
    let artifact = t
        .publish(
            &key,
            &format!("Inline clone {view}"),
            "<!doctype html><h1>Clone folders</h1>",
            None,
        )
        .await?;
    let artifact_id = id_of(&artifact);
    let source = t
        .api(
            "POST",
            "/collections",
            Some(json!({
                "org": t.org,
                "name": format!("Inline clone source {view}"),
                "artifactIds": [artifact_id],
            })),
        )
        .await?;
    let destination = t
        .api(
            "POST",
            "/collections",
            Some(json!({"org": t.org, "name": format!("Inline clone destination {view}")})),
        )
        .await?;
    assert!(
        source.ok() && destination.ok(),
        "{} {}",
        source.status,
        destination.status
    );
    let source_id = id_of(&source.json());
    let target_id = id_of(&destination.json());
    page.goto(&format!(
        "/?libraryView={view}&org={}",
        encode_component(&t.org)
    ))
    .await?;
    if view == "reel" {
        page.locator(&format!("[data-collection-peek=\"{source_id}\"]"))
            .click()
            .await?;
    }
    let scope = if view == "reel" {
        ".collection-reel".to_owned()
    } else {
        format!("[data-ribbon-id=\"{source_id}\"]")
    };
    let card = page.locator(&format!("{scope} .card[data-id=\"{artifact_id}\"]"));
    card.locator("[data-action=\"more\"]").click().await?;
    card.locator("[data-collection-folder-picker]")
        .click()
        .await?;
    let checkbox = card.locator(&format!("[data-collection-folder-option=\"{target_id}\"]"));
    checkbox.check().await?;
    poll_membership(&t, &target_id, &artifact_id, true).await?;
    expect(&checkbox).to_be_enabled().await?;
    expect(&checkbox).to_be_visible().await?;
    checkbox.uncheck().await?;
    poll_membership(&t, &target_id, &artifact_id, false).await?;
    expect(&checkbox).to_be_enabled().await?;
    expect(&checkbox).not().to_be_checked().await
}

async fn new_folder_cancel(t: Ctx) -> Result<(), Error> {
    new_folder_returns(t, "Cancel").await
}

async fn new_folder_close(t: Ctx) -> Result<(), Error> {
    new_folder_returns(t, "Close").await
}

async fn new_folder_escape(t: Ctx) -> Result<(), Error> {
    new_folder_returns(t, "Escape").await
}

/// `new-folder ${close} returns to the expanded artifact popup`.
async fn new_folder_returns(t: Ctx, close: &str) -> Result<(), Error> {
    let page = t.page().await?;
    let Opened { card, panel, .. } = fixture(&t, &page).await?;
    panel.locator("[data-picker-new]").click().await?;
    let dialog = page.get_by_role(Role::new("dialog").exact_name("Create a folder"));
    expect(&dialog).to_be_visible().await?;
    if close == "Escape" {
        page.keyboard().press("Escape").await?;
    } else {
        dialog
            .get_by_role(Role::new("button").exact_name(close))
            .click()
            .await?;
    }
    expect(&dialog).to_have_count(0).await?;
    expect(&panel).to_be_visible().await?;
    expect(&card.locator(".card-menu")).to_be_visible().await
}

async fn mobile_viewport(t: Ctx) -> Result<(), Error> {
    let page = t.page().await?;
    page.set_viewport(320, 568, 1.0).await?;
    let opened = fixture(&t, &page).await?;
    let (card, panel) = (&opened.card, &opened.panel);
    expect(panel).to_be_visible().await?;
    let selected = card.locator(&format!(
        "[data-collection-folder-option=\"{}\"]",
        opened.folder_id
    ));
    selected.check().await?;
    expect(&selected).to_be_enabled().await?;
    let menu = card.locator(".card-menu");
    let bounds = menu
        .bounding_box()
        .await?
        .expect("the card menu has a bounding box");
    assert!(bounds.x >= 0.0, "{bounds:?}");
    assert!(bounds.y >= 0.0, "{bounds:?}");
    assert!(bounds.x + bounds.width <= 320.0, "{bounds:?}");
    assert!(bounds.y + bounds.height <= 568.0, "{bounds:?}");
    let overflow = |value: Value| value.as_f64().unwrap_or(f64::INFINITY);
    let panel_overflow = overflow(
        panel
            .evaluate("(node) => node.scrollWidth - node.clientWidth", Value::Null)
            .await?,
    );
    assert!(panel_overflow <= 1.0, "panel overflow {panel_overflow}");
    let menu_overflow = overflow(
        menu.evaluate("(node) => node.scrollWidth - node.clientWidth", Value::Null)
            .await?,
    );
    assert!(menu_overflow <= 1.0, "menu overflow {menu_overflow}");
    Ok(())
}
