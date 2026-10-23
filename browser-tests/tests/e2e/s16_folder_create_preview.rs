// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
//! `create-folder wireframe`, from `playwright/tests/16-folder-create-preview.spec.mjs`.

use browser_tests::e2e::{Case, Ctx, encode_component, now_millis};
use browser_tests::{Error, Locator, Page, Role, e2e_case, exact, expect, poll, regex};
use serde_json::{Value, json};

const SPEC: &str = "16-folder-create-preview";

pub fn cases() -> Vec<Case> {
    vec![
        e2e_case!(
            SPEC,
            "shows a live folder preview while editing name, context, and color",
            live_preview
        ),
        e2e_case!(
            SPEC,
            "admin organization picker contains concrete organizations only",
            concrete_organizations
        ),
        e2e_case!(
            SPEC,
            "mobile create dialog keeps the preview and actions reachable",
            mobile_dialog
        ),
        e2e_case!(
            SPEC,
            "editing preserves a custom color, explicit cover, and context",
            editing_preserves
        ),
        e2e_case!(
            SPEC,
            "the pastel preview stays readable in dark theme",
            dark_theme_preview
        ),
    ]
}

fn html(title: &str) -> String {
    format!(
        "<!doctype html><meta name=\"viewport\" content=\"width=device-width\"><main><h1>{title}</h1></main>"
    )
}

/// Open the Reel Shelf for every organization and the "Create a folder" dialog.
async fn open_admin_shelf(page: &Page) -> Result<Locator<'_>, Error> {
    page.goto("/?libraryView=reel&org=all").await?;
    expect(&page.locator("#new-folder")).to_be_visible().await?;
    page.locator("#new-folder").click().await?;
    Ok(page.get_by_role(Role::new("dialog").exact_name("Create a folder")))
}

async fn live_preview(t: Ctx) -> Result<(), Error> {
    let key = t.publisher_key().await?;
    let page = t.page().await?;
    t.publish(&key, "Preview source", &html("Preview source"), None)
        .await?;
    let dialog = open_admin_shelf(&page).await?;

    expect(&dialog.get_by_label("Folder name"))
        .to_be_focused()
        .await?;
    expect(&dialog.get_by_text(regex("your folder, as it will look", "i")))
        .to_be_visible()
        .await?;

    dialog
        .get_by_label("Folder name")
        .fill("Interface experiments")
        .await?;
    dialog
        .get_by_label("A little context")
        .fill("Pages that explore interaction patterns.")
        .await?;
    let preview =
        dialog.locator("#folder-live-preview, [data-folder-live-preview], .create-preview");
    expect(&preview)
        .to_contain_text("Interface experiments")
        .await?;
    expect(&dialog.get_by_label("A little context"))
        .to_have_value("Pages that explore interaction patterns.")
        .await?;
    dialog
        .get_by_label("Organization")
        .select_option(&[t.org.as_str()])
        .await?;
    expect(&preview).to_contain_text(t.org.as_str()).await?;

    let swatches = dialog.locator(
        ".collection-color-option input[type=\"radio\"], [data-color-swatch], [data-folder-color]",
    );
    expect(&swatches).to_have_count(4).await?;
    dialog.get_by_text(exact("Slate")).click().await?;
    expect(&swatches.nth(2)).to_be_checked().await?;
    swatches.nth(2).focus().await?;
    page.keyboard().press("ArrowRight").await?;
    expect(&swatches.nth(3)).to_be_checked().await?;

    dialog
        .get_by_role(Role::new("button").exact_name("Cancel"))
        .click()
        .await?;
    expect(&dialog).to_have_count(0).await
}

async fn concrete_organizations(t: Ctx) -> Result<(), Error> {
    let page = t.page().await?;
    let org = t.org.clone();
    let dialog = open_admin_shelf(&page).await?;
    let organization = dialog.get_by_label("Organization");
    expect(&organization).to_be_visible().await?;

    // `organization.locator("option").evaluateAll(...)`: the select's own options.
    let values = organization
        .evaluate(
            "(select) => [...select.querySelectorAll('option')].map(option => option.value)",
            Value::Null,
        )
        .await?;
    let values: Vec<String> = values
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect();
    assert!(values.contains(&org), "{values:?}");
    assert!(!values.iter().any(|value| value == "admin"), "{values:?}");
    assert!(!values.iter().any(|value| value == "all"), "{values:?}");
    let unique: std::collections::HashSet<&String> = values.iter().collect();
    assert_eq!(unique.len(), values.len(), "{values:?}");

    organization.select_option(&[org.as_str()]).await?;
    let folder_name = format!("Admin preview {}", now_millis());
    dialog
        .get_by_label("Folder name")
        .fill(&folder_name)
        .await?;
    dialog
        .get_by_role(Role::new("button").exact_name("Create folder"))
        .click()
        .await?;
    expect(&dialog).to_have_count(0).await?;

    let path = format!(
        "/collections?org={}&include=projection",
        encode_component(&org)
    );
    poll(
        "the new folder's organization",
        || async {
            let body = t.api("GET", &path, None).await?.json();
            Ok(body["collections"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|collection| collection["name"] == json!(folder_name))
                .and_then(|collection| collection["org"].as_str())
                .unwrap_or("")
                .to_owned())
        },
        |found| *found == org,
    )
    .await?;
    Ok(())
}

async fn mobile_dialog(t: Ctx) -> Result<(), Error> {
    let page = t.page().await?;
    page.set_viewport(375, 667, 1.0).await?;
    let dialog = open_admin_shelf(&page).await?;
    expect(&dialog).to_be_visible().await?;

    let bounds = dialog.bounding_box().await?.expect("the dialog has a box");
    assert!(bounds.x >= 0.0, "{bounds:?}");
    assert!(bounds.x + bounds.width <= 375.0, "{bounds:?}");
    let cancel = dialog.get_by_role(Role::new("button").exact_name("Cancel"));
    let create = dialog.get_by_role(Role::new("button").exact_name("Create folder"));
    expect(&cancel).to_be_visible().await?;
    expect(&create).to_be_visible().await?;
    let create_box = create
        .bounding_box()
        .await?
        .expect("Create folder has a box");
    assert!(create_box.y + create_box.height <= 667.0, "{create_box:?}");
    let preview = dialog.locator("[data-folder-live-preview]");
    preview.scroll_into_view_if_needed().await?;
    let preview_box = preview
        .bounding_box()
        .await?
        .expect("the preview has a box");
    assert!(preview_box.y >= 0.0, "{preview_box:?}");
    assert!(
        preview_box.y + preview_box.height < create_box.y,
        "{preview_box:?} {create_box:?}"
    );
    expect(&cancel).to_be_in_viewport().await
}

async fn editing_preserves(t: Ctx) -> Result<(), Error> {
    let key = t.publisher_key().await?;
    let page = t.page().await?;
    let org = t.org.clone();
    let artifact = t
        .publish(&key, "Custom cover", &html("Custom cover"), None)
        .await?;
    let artifact_id = artifact["id"].as_str().unwrap_or("").to_owned();
    let result = t
        .api(
            "POST",
            "/collections",
            Some(json!({
                "org": org,
                "name": "Custom palette",
                "description": "Saved context",
                "color": "#102030",
                "artifactIds": [artifact_id],
                "coverArtifactId": artifact_id,
            })),
        )
        .await?;
    assert!(result.ok(), "{} {}", result.status, result.text());
    let folder = result.json();
    let folder_id = folder["id"].as_str().unwrap_or("").to_owned();
    page.goto(&format!(
        "/?libraryView=reel&org={}",
        encode_component(&org)
    ))
    .await?;
    page.locator(&format!("[data-collection-peek=\"{folder_id}\"]"))
        .hover()
        .await?;
    page.locator(".collection-reel [data-collection-edit]")
        .click()
        .await?;
    let dialog = page.get_by_role(Role::new("dialog").exact_name("Edit folder"));
    expect(&dialog.get_by_role(Role::new("radio").exact_name("Custom")))
        .to_be_checked()
        .await?;
    expect(&dialog.get_by_label("Cover preview"))
        .to_have_value(artifact_id.as_str())
        .await?;
    expect(&dialog.get_by_label("Description"))
        .to_have_value("Saved context")
        .await?;
    expect(&dialog.locator("[data-folder-live-preview]"))
        .to_contain_text("1 artifact")
        .await?;
    dialog
        .get_by_label("Folder name")
        .fill("Renamed custom palette")
        .await?;
    dialog
        .get_by_role(Role::new("button").name("Save changes"))
        .click()
        .await?;
    expect(&dialog).to_have_count(0).await?;
    let projection = t
        .api(
            "GET",
            &format!("/collections?org={}", encode_component(&org)),
            None,
        )
        .await?
        .json();
    let row = projection["collections"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|row| row["id"] == json!(folder_id))
        .cloned()
        .unwrap_or(Value::Null);
    // `toMatchObject`
    for (field, wanted) in [
        ("name", json!("Renamed custom palette")),
        ("color", json!("#102030")),
        ("description", json!("Saved context")),
        ("coverArtifactId", json!(artifact_id)),
    ] {
        assert_eq!(row[field], wanted, "{field} of {row}");
    }
    Ok(())
}

async fn dark_theme_preview(t: Ctx) -> Result<(), Error> {
    let page = t.page().await?;
    open_admin_shelf(&page).await?;
    page.evaluate("document.documentElement.dataset.theme = 'dark'")
        .await?;
    let face = page.locator(".collection-preview-face");
    expect(&face.locator("strong"))
        .to_have_css("color", "rgb(23, 35, 45)")
        .await?;
    page.get_by_role(Role::new("dialog").exact_name("Create a folder"))
        .get_by_role(Role::new("button").exact_name("Cancel"))
        .click()
        .await
}
