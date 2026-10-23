// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
//! `collection gestures`, from `playwright/tests/14-collection-gestures.spec.mjs`: real pointer
//! drags (with Playwright's drag-and-drop interception, [`browser_tests::DragMouse`]), touch taps
//! and hover placement of the Reel Shelf.

use std::path::PathBuf;

use base64::Engine;
use browser_tests::e2e::{Case, Ctx, encode_component};
use browser_tests::{BoundingBox, Error, Locator, Page, e2e_case, expect, poll, regex};
use serde_json::{Value, json};

const SPEC: &str = "14-collection-gestures";

pub fn cases() -> Vec<Case> {
    vec![
        e2e_case!(
            SPEC,
            "real pointer drag adds a reference and preserves the artifact body",
            pointer_drag_adds_reference
        ),
        e2e_case!(
            SPEC,
            "real pointer drag reorders ribbons above and below and persists after reload",
            pointer_drag_reorders_ribbons
        ),
        e2e_case!(
            SPEC,
            "touch tap pins then dismisses the Reel Shelf and keeps touch targets usable",
            touch_tap_pins_and_dismisses
        ),
        e2e_case!(
            SPEC,
            "real hover places the Reel Shelf above or below after scrolling",
            hover_places_reel
        ),
    ]
}

fn html(title: &str) -> String {
    format!(
        "<!doctype html><meta charset=\"utf-8\"><main><h1>{title}</h1><p>Gesture QA artifact.</p></main>"
    )
}

fn id_of(value: &Value) -> String {
    value["id"].as_str().unwrap_or("").to_owned()
}

async fn projection(t: &Ctx) -> Result<Value, Error> {
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

/// The `artifactIds` of collection `id` in a projection.
fn artifact_ids(projection: &Value, id: &str) -> Option<Vec<String>> {
    projection["collections"]
        .as_array()?
        .iter()
        .find(|row| row["id"] == json!(id))
        .map(|row| {
            row["artifactIds"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
}

async fn bounds(locator: &Locator<'_>) -> Result<BoundingBox, Error> {
    locator
        .bounding_box()
        .await?
        .ok_or_else(|| Error::Assertion(format!("{} has no bounding box", locator.describe())))
}

/// `page.screenshot({ path: testInfo.outputPath(name), fullPage: true })`.
async fn full_page_screenshot(page: &Page, name: &str) -> Result<PathBuf, Error> {
    let metrics = page.command("Page.getLayoutMetrics", json!({})).await?;
    let size = &metrics["cssContentSize"];
    let shot = page
        .command(
            "Page.captureScreenshot",
            json!({
                "format": "png",
                "captureBeyondViewport": true,
                "clip": {
                    "x": 0,
                    "y": 0,
                    "width": size["width"].as_f64().unwrap_or(1280.0).ceil(),
                    "height": size["height"].as_f64().unwrap_or(720.0).ceil(),
                    "scale": 1,
                },
            }),
        )
        .await?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(shot["data"].as_str().unwrap_or(""))
        .map_err(|error| Error::Protocol(format!("screenshot: {error}")))?;
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("e2e-output")
        .join(SPEC);
    let path = dir.join(name);
    std::fs::create_dir_all(&dir)
        .and_then(|()| std::fs::write(&path, bytes))
        .map_err(|error| Error::Protocol(format!("{}: {error}", path.display())))?;
    Ok(path)
}

async fn pointer_drag_adds_reference(t: Ctx) -> Result<(), Error> {
    let key = t.publisher_key().await?;
    let page = t.page().await?;
    let org = t.org.clone();
    let artifact = t
        .publish(
            &key,
            &format!("Gesture drag {org}"),
            &html("Drag source"),
            None,
        )
        .await?;
    let artifact_id = id_of(&artifact);
    let raw = format!("/raw/{artifact_id}");
    let before = t.request().get(&raw).await?.text();
    let source = t
        .api(
            "POST",
            "/collections",
            Some(json!({"org": org, "name": format!("Gesture source {org}"), "artifactIds": [artifact_id]})),
        )
        .await?;
    let target = t
        .api(
            "POST",
            "/collections",
            Some(json!({"org": org, "name": format!("Gesture target {org}")})),
        )
        .await?;
    assert!(
        source.ok() && target.ok(),
        "{} {}",
        source.status,
        target.status
    );
    let source_id = id_of(&source.json());
    let target_id = id_of(&target.json());
    page.set_viewport(1440, 1000, 1.0).await?;
    page.goto(&format!(
        "/?libraryView=reel&org={}",
        encode_component(&org)
    ))
    .await?;
    let source_face = page.locator(&format!("[data-collection-peek=\"{source_id}\"]"));
    source_face.hover().await?;
    page.locator("[data-reel-pin]").click().await?;
    page.locator(&format!("[data-collection-id=\"{target_id}\"]"))
        .scroll_into_view_if_needed()
        .await?;
    page.locator("[data-reel-pin]").click().await?;
    expect(&page.locator(".collection-reel:not([hidden])"))
        .not()
        .to_have_class(regex("is-pinned", ""))
        .await?;
    let card = page.locator(&format!(
        ".collection-reel-card [data-collection-artifact=\"{artifact_id}\"]"
    ));
    let target_face = page.locator(&format!("[data-collection-id=\"{target_id}\"]"));
    expect(&card).to_be_visible().await?;
    let preview = card.locator(".preview a[aria-label^=\"Open \"]");
    let source_box = bounds(&preview).await?;
    let destination_box = bounds(&target_face).await?;
    assert!(destination_box.x >= 0.0, "{destination_box:?}");
    assert!(
        destination_box.x + destination_box.width <= 1440.0,
        "{destination_box:?}"
    );
    let hit = target_face
        .evaluate(
            "node => { const box = node.getBoundingClientRect(); return document.elementFromPoint(box.x + box.width / 2, box.y + box.height / 2)?.closest('[data-collection-id]')?.dataset.collectionId; }",
            Value::Null,
        )
        .await?;
    assert_eq!(hit, json!(target_id));
    let (start_x, start_y) = (
        source_box.x + source_box.width / 2.0,
        source_box.y + source_box.height / 2.0,
    );
    let mut mouse = page.drag_mouse();
    mouse.move_to(start_x, start_y, 1).await?;
    mouse.down().await?;
    mouse.move_to(start_x + 10.0, start_y + 10.0, 3).await?;
    mouse
        .move_to(
            destination_box.x + destination_box.width / 2.0,
            destination_box.y + destination_box.height / 2.0,
            12,
        )
        .await?;
    mouse.up().await?;
    poll(
        "the target folder references the artifact",
        || async { Ok(artifact_ids(&projection(&t).await?, &target_id)) },
        |ids| ids.as_ref().is_some_and(|ids| ids.contains(&artifact_id)),
    )
    .await?;
    let after = t.request().get(&raw).await?.text();
    assert_eq!(after, before);
    let source_ids = artifact_ids(&projection(&t).await?, &source_id).unwrap_or_default();
    assert!(source_ids.contains(&artifact_id), "{source_ids:?}");
    full_page_screenshot(&page, "pointer-drag.png").await?;
    Ok(())
}

async fn pointer_drag_reorders_ribbons(t: Ctx) -> Result<(), Error> {
    let key = t.publisher_key().await?;
    let page = t.page().await?;
    let org = t.org.clone();
    let artifact = t
        .publish(
            &key,
            &format!("Ribbon gesture {org}"),
            &html("Ribbon"),
            None,
        )
        .await?;
    let artifact_id = id_of(&artifact);
    let mut folder_ids = Vec::new();
    for name in ["Gesture ribbon A", "Gesture ribbon B", "Gesture ribbon C"] {
        let response = t
            .api(
                "POST",
                "/collections",
                Some(json!({"org": org, "name": name, "artifactIds": [artifact_id]})),
            )
            .await?;
        assert!(response.ok(), "{} {}", response.status, response.text());
        folder_ids.push(id_of(&response.json()));
    }
    let preferences = t
        .api(
            "PUT",
            &format!("/gallery/preferences?org={}", encode_component(&org)),
            Some(json!({
                "view": "ribbons",
                "previewSize": "compact",
                "artifactLayout": "grid",
                "collectionOrderByOrg": {org.as_str(): folder_ids},
                "collapsedCollectionIdsByOrg": {org.as_str(): folder_ids},
            })),
        )
        .await?;
    assert!(
        preferences.ok(),
        "{} {}",
        preferences.status,
        preferences.text()
    );
    page.set_viewport(1440, 900, 1.0).await?;
    page.goto(&format!(
        "/?libraryView=ribbons&org={}",
        encode_component(&org)
    ))
    .await?;
    let ribbon = |id: &str| page.locator(&format!("[data-ribbon-id=\"{id}\"]"));
    let (first, second, third) = (
        ribbon(&folder_ids[0]),
        ribbon(&folder_ids[1]),
        ribbon(&folder_ids[2]),
    );
    expect(&first).to_have_count(1).await?;
    expect(&second).to_have_count(1).await?;
    expect(&third).to_have_count(1).await?;
    let ids = json!(folder_ids);
    let scoped_order = || {
        let ids = ids.clone();
        let page = &page;
        async move {
            page.call(
                "(ids) => [...document.querySelectorAll('[data-ribbon-id]')].map((item) => item.dataset.ribbonId).filter((id) => ids.includes(id))",
                &[ids],
            )
            .await
        }
    };
    first.scroll_into_view_if_needed().await?;
    third.scroll_into_view_if_needed().await?;
    let third_box = bounds(&third).await?;
    let grip_box = bounds(&first.locator("[data-ribbon-drag]")).await?;
    let (grip_x, grip_y) = (
        grip_box.x + grip_box.width / 2.0,
        grip_box.y + grip_box.height / 2.0,
    );
    let mut mouse = page.drag_mouse();
    mouse.move_to(grip_x, grip_y, 1).await?;
    mouse.down().await?;
    mouse.move_to(grip_x + 8.0, grip_y + 8.0, 1).await?;
    mouse
        .move_to(
            third_box.x + third_box.width / 2.0,
            third_box.y + third_box.height - 2.0,
            12,
        )
        .await?;
    mouse.up().await?;
    let moved_down = json!([folder_ids[1], folder_ids[2], folder_ids[0]]);
    poll("the ribbon order", scoped_order, |order| {
        *order == moved_down
    })
    .await?;
    let first_row = ribbon(&folder_ids[1]);
    let moved = ribbon(&folder_ids[0]);
    moved.scroll_into_view_if_needed().await?;
    first_row.scroll_into_view_if_needed().await?;
    let target_box = bounds(&first_row).await?;
    let moved_grip = bounds(&moved.locator("[data-ribbon-drag]")).await?;
    let (grip_x, grip_y) = (
        moved_grip.x + moved_grip.width / 2.0,
        moved_grip.y + moved_grip.height / 2.0,
    );
    let mut mouse = page.drag_mouse();
    mouse.move_to(grip_x, grip_y, 1).await?;
    mouse.down().await?;
    mouse.move_to(grip_x + 8.0, grip_y + 8.0, 1).await?;
    mouse
        .move_to(
            target_box.x + target_box.width / 2.0,
            target_box.y + 2.0,
            12,
        )
        .await?;
    mouse.up().await?;
    let original = json!(folder_ids);
    poll("the ribbon order", scoped_order, |order| *order == original).await?;
    let final_order = scoped_order().await?;
    let preferences_path = format!("/gallery/preferences?org={}", encode_component(&org));
    poll(
        "the saved folder order",
        || async {
            let saved = t.api("GET", &preferences_path, None).await?.json();
            let order: Vec<Value> = saved["collectionOrderByOrg"][org.as_str()]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|id| original.as_array().is_some_and(|ids| ids.contains(id)))
                .cloned()
                .collect();
            Ok(Value::Array(order))
        },
        |order| *order == final_order,
    )
    .await?;
    page.reload().await?;
    poll("the ribbon order after reload", scoped_order, |order| {
        *order == final_order
    })
    .await?;
    Ok(())
}

async fn touch_tap_pins_and_dismisses(t: Ctx) -> Result<(), Error> {
    let key = t.publisher_key().await?;
    let page = t.page().await?;
    // `test.use({ hasTouch: true, viewport: { width: 390, height: 760 } })`
    page.set_viewport(390, 760, 1.0).await?;
    page.set_has_touch(true).await?;
    let org = t.org.clone();
    let artifact = t
        .publish(&key, &format!("Touch gesture {org}"), &html("Touch"), None)
        .await?;
    let folder_response = t
        .api(
            "POST",
            "/collections",
            Some(json!({"org": org, "name": format!("Touch folder {org}"), "artifactIds": [id_of(&artifact)]})),
        )
        .await?;
    assert!(
        folder_response.ok(),
        "{} {}",
        folder_response.status,
        folder_response.text()
    );
    page.goto(&format!(
        "/?libraryView=reel&org={}",
        encode_component(&org)
    ))
    .await?;
    let folder = page
        .locator("[data-collection-peek]")
        .filter_has_text(format!("Touch folder {org}"));
    folder.tap().await?;
    expect(&page.locator(".collection-reel.is-pinned"))
        .to_be_visible()
        .await?;
    folder.tap().await?;
    expect(&page.locator(".collection-reel:not([hidden])"))
        .to_have_count(0)
        .await?;
    let undersized = page
        .evaluate(
            "[...document.querySelectorAll(\".collection-surface[data-view='reel'] button\")].map((button) => ({ label: button.getAttribute('aria-label') || button.textContent.trim(), box: button.getBoundingClientRect().toJSON() })).filter((item) => item.box.width > 0 && item.box.height > 0 && (item.box.width < 44 || item.box.height < 44))",
        )
        .await?;
    assert_eq!(undersized, json!([]), "{undersized}");
    let overflow = page
        .evaluate("document.documentElement.scrollWidth - document.documentElement.clientWidth")
        .await?;
    assert_eq!(overflow, json!(0));
    Ok(())
}

async fn hover_places_reel(t: Ctx) -> Result<(), Error> {
    let key = t.publisher_key().await?;
    let page = t.page().await?;
    let org = t.org.clone();
    let artifact = t
        .publish(
            &key,
            &format!("Scroll gesture {org}"),
            &html("Scroll"),
            None,
        )
        .await?;
    let created = t
        .api(
            "POST",
            "/collections",
            Some(json!({"org": org, "name": format!("Scroll folder {org}"), "artifactIds": [id_of(&artifact)]})),
        )
        .await?;
    assert!(created.ok(), "{} {}", created.status, created.text());
    page.set_viewport(900, 520, 1.0).await?;
    page.goto(&format!(
        "/?libraryView=reel&org={}",
        encode_component(&org)
    ))
    .await?;
    let folder = page
        .locator("[data-collection-peek]")
        .filter_has_text(format!("Scroll folder {org}"));
    folder.hover().await?;
    let reel = page.locator(".collection-reel:not([hidden])");
    expect(&reel).to_be_visible().await?;
    for y in [0, 450, 900] {
        page.call("(scrollY) => window.scrollTo(0, scrollY)", &[json!(y)])
            .await?;
        folder.hover().await?;
        // `expect(box).toBeTruthy()`
        let reel_box = bounds(&reel).await?;
        assert!(reel_box.y >= 0.0, "{y}: {reel_box:?}");
        assert!(reel_box.y + reel_box.height <= 520.0, "{y}: {reel_box:?}");
        let placement = reel.get_attribute("data-placement").await?;
        assert!(
            placement
                .as_deref()
                .is_some_and(|placement| placement.contains("above") || placement.contains("below")),
            "{y}: data-placement {placement:?}"
        );
    }
    Ok(())
}
