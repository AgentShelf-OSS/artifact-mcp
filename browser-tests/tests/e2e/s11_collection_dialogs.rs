// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
//! `collection dialog dismissal and validation`, from
//! `playwright/tests/11-collection-dialogs.spec.mjs`: every way out of the New folder, Edit folder
//! and Delete folder dialogs, focus return, validation and failed writes.

use std::future::Future;
use std::time::Duration;

use browser_tests::e2e::{Case, Ctx, encode_component, now_millis};
use browser_tests::{Error, LogEntry, Page, Response, Role, e2e_case, expect, poll};
use serde_json::{Value, json};

const SPEC: &str = "11-collection-dialogs";

pub fn cases() -> Vec<Case> {
    vec![
        e2e_case!(
            SPEC,
            "Cancel closes New folder without validation, network mutation, or browser errors",
            cancel_closes_new_folder
        ),
        e2e_case!(
            SPEC,
            "X closes New folder and restores focus to its launcher",
            x_closes_new_folder
        ),
        e2e_case!(
            SPEC,
            "the shelf New folder tile opens and cancels the same accessible dialog",
            shelf_tile_dialog
        ),
        e2e_case!(
            SPEC,
            "Escape closes New folder without a mutation and restores focus",
            escape_closes_new_folder
        ),
        e2e_case!(
            SPEC,
            "required folder name blocks invalid submission and valid submission creates one folder",
            required_name
        ),
        e2e_case!(
            SPEC,
            "cancelling nested New folder returns to the folder picker without a mutation",
            nested_new_folder_cancel
        ),
        e2e_case!(
            SPEC,
            "edit folder Cancel exits after clearing its required name",
            edit_cancel
        ),
        e2e_case!(
            SPEC,
            "edit folder Close exits after clearing its required name",
            edit_close
        ),
        e2e_case!(
            SPEC,
            "edit folder Escape exits after clearing its required name",
            edit_escape
        ),
        e2e_case!(
            SPEC,
            "Enter submits the primary create action once",
            enter_submits_once
        ),
        e2e_case!(
            SPEC,
            "failed creation retains the draft and can be cancelled without another write",
            failed_creation_keeps_draft
        ),
        e2e_case!(
            SPEC,
            "delete confirmation Cancel retains the edit dialog and its draft",
            delete_confirmation_cancel
        ),
        e2e_case!(
            SPEC,
            "delete confirmation Close retains the edit dialog and its draft",
            delete_confirmation_close
        ),
        e2e_case!(
            SPEC,
            "folder deletion confirms in an accessible dialog and preserves its artifacts",
            deletion_preserves_artifacts
        ),
        e2e_case!(
            SPEC,
            "Escape cancels folder deletion without a request",
            escape_cancels_deletion
        ),
        e2e_case!(
            SPEC,
            "failed folder deletion keeps the confirmation open and retryable",
            failed_deletion_retryable
        ),
    ]
}

fn html(title: &str) -> String {
    format!(
        "<!doctype html><meta name=\"viewport\" content=\"width=device-width\"><main><h1>{title}</h1></main>"
    )
}

fn id_of(value: &Value) -> String {
    value["id"].as_str().unwrap_or("").to_owned()
}

fn strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect()
}

/// `new URL(url).pathname`.
fn pathname(url: &str) -> &str {
    let without_query = url.split(['?', '#']).next().unwrap_or("");
    match without_query.find("://") {
        Some(scheme_end) => {
            let rest = &without_query[scheme_end + 3..];
            rest.find('/').map_or("/", |slash| &rest[slash..])
        }
        None => without_query,
    }
}

async fn sleep_ms(ms: u64) {
    tokio::time::sleep(Duration::from_millis(ms)).await;
}

/// `openLibrary`.
async fn open_library(page: &Page, org: &str) -> Result<(), Error> {
    page.goto(&format!("/?libraryView=reel&org={}", encode_component(org)))
        .await?;
    expect(&page.locator("#new-folder")).to_be_visible().await
}

/// The `beforeEach` hook: reset the library preferences and open the library.
async fn before_each(t: &Ctx) -> Result<Page, Error> {
    let page = t.page().await?;
    let org = t.org.clone();
    t.api(
        "PUT",
        &format!("/gallery/preferences?org={}", encode_component(&org)),
        Some(json!({
            "view": "reel",
            "previewSize": "compact",
            "artifactLayout": "grid",
            "collectionOrderByOrg": {org.clone(): []},
            "collapsedCollectionIdsByOrg": {org.clone(): []},
        })),
    )
    .await?;
    open_library(&page, &org).await?;
    Ok(page)
}

/// `expectNoMutation`: `action` and the 100 ms after it send no POST, PUT, PATCH or DELETE.
async fn expect_no_mutation(
    page: &Page,
    action: impl Future<Output = Result<(), Error>>,
) -> Result<(), Error> {
    let seen = page.requests().len();
    action.await?;
    sleep_ms(100).await;
    let mutations: Vec<String> = page.requests()[seen..]
        .iter()
        .filter(|request| ["POST", "PUT", "PATCH", "DELETE"].contains(&request.method.as_str()))
        .map(|request| format!("{} {}", request.method, pathname(&request.url)))
        .collect();
    assert_eq!(mutations, Vec::<String>::new());
    Ok(())
}

/// `expectNoBrowserErrors`: `action` and the 100 ms after it raise no uncaught exception
/// (`pageerror`) and log no console error.
async fn expect_no_browser_errors(
    page: &Page,
    action: impl Future<Output = Result<(), Error>>,
) -> Result<(), Error> {
    let mark = page.log_mark();
    action.await?;
    sleep_ms(100).await;
    let errors: Vec<String> = page
        .log_since(mark)
        .into_iter()
        .filter_map(|entry| match entry {
            LogEntry::Exception { text, .. } => Some(format!("pageerror: {text}")),
            LogEntry::Console { level, text, .. } if level == "error" => {
                Some(format!("console: {text}"))
            }
            _ => None,
        })
        .collect();
    assert_eq!(errors, Vec::<String>::new());
    Ok(())
}

/// `GET /collections?org=…` (`include=projection` when `projection`): the `collections` rows.
async fn collections(t: &Ctx, projection: bool) -> Result<Vec<Value>, Error> {
    let suffix = if projection {
        "&include=projection"
    } else {
        ""
    };
    let response = t
        .api(
            "GET",
            &format!("/collections?org={}{suffix}", encode_component(&t.org)),
            None,
        )
        .await?;
    Ok(response.json()["collections"]
        .as_array()
        .cloned()
        .unwrap_or_default())
}

/// Create an empty folder named `name` in the run organization; returns its JSON.
async fn create_folder(t: &Ctx, body: Value) -> Result<(bool, Value), Error> {
    let created = t.api("POST", "/collections", Some(body)).await?;
    Ok((created.ok(), created.json()))
}

/// Reload, hover the folder's shelf face and open its Edit folder dialog.
async fn open_edit(page: &Page, folder_id: &str) -> Result<(), Error> {
    page.reload().await?;
    page.locator(&format!("[data-collection-peek=\"{folder_id}\"]"))
        .hover()
        .await?;
    page.locator(".collection-reel [data-collection-edit]")
        .click()
        .await
}

async fn cancel_closes_new_folder(t: Ctx) -> Result<(), Error> {
    let page = before_each(&t).await?;
    page.locator("#new-folder").click().await?;
    let dialog = page.locator(".collection-create-dialog");
    expect(&dialog).to_be_visible().await?;
    let name = dialog.locator("input[name=name]");
    expect(&name).to_be_focused().await?;
    expect_no_browser_errors(
        &page,
        expect_no_mutation(
            &page,
            dialog
                .get_by_role(Role::new("button").name("Cancel"))
                .click(),
        ),
    )
    .await?;
    expect(&dialog).to_have_count(0).await?;
    expect(&page.locator("#new-folder")).to_be_focused().await
}

async fn x_closes_new_folder(t: Ctx) -> Result<(), Error> {
    let page = before_each(&t).await?;
    page.locator("#new-folder").click().await?;
    let dialog = page.locator(".collection-create-dialog");
    expect(&dialog).to_be_visible().await?;
    expect_no_browser_errors(
        &page,
        expect_no_mutation(
            &page,
            dialog
                .get_by_role(Role::new("button").name("Close"))
                .click(),
        ),
    )
    .await?;
    expect(&dialog).to_have_count(0).await?;
    expect(&page.locator("#new-folder")).to_be_focused().await
}

async fn shelf_tile_dialog(t: Ctx) -> Result<(), Error> {
    let page = before_each(&t).await?;
    let tile = page.locator(".collection-new-tile");
    tile.click().await?;
    let dialog = page.get_by_role(Role::new("dialog").exact_name("Create a folder"));
    expect(&dialog.get_by_label("Folder name"))
        .to_be_focused()
        .await?;
    expect_no_mutation(
        &page,
        dialog
            .get_by_role(Role::new("button").exact_name("Cancel"))
            .click(),
    )
    .await?;
    expect(&dialog).to_have_count(0).await?;
    expect(&tile).to_be_focused().await
}

async fn escape_closes_new_folder(t: Ctx) -> Result<(), Error> {
    let page = before_each(&t).await?;
    page.locator("#new-folder").click().await?;
    let dialog = page.locator(".collection-create-dialog");
    expect(&dialog).to_be_visible().await?;
    expect_no_browser_errors(
        &page,
        expect_no_mutation(&page, page.keyboard().press("Escape")),
    )
    .await?;
    expect(&dialog).to_have_count(0).await?;
    expect(&page.locator("#new-folder")).to_be_focused().await
}

async fn required_name(t: Ctx) -> Result<(), Error> {
    let page = before_each(&t).await?;
    page.locator("#new-folder").click().await?;
    let dialog = page.locator(".collection-create-dialog");
    let name = dialog.locator("input[name=name]");
    dialog
        .get_by_role(Role::new("button").name("Create folder"))
        .click()
        .await?;
    expect(&dialog).to_be_visible().await?;
    let message = name
        .evaluate("(input) => input.validationMessage", Value::Null)
        .await?;
    // `/fill out|complete/i`
    let lower = message.as_str().unwrap_or("").to_lowercase();
    assert!(
        lower.contains("fill out") || lower.contains("complete"),
        "{message}"
    );

    let folder_name = format!("Dialog valid {}", now_millis());
    name.fill(&folder_name).await?;
    expect_no_browser_errors(&page, async {
        dialog
            .get_by_role(Role::new("button").name("Create folder"))
            .click()
            .await?;
        expect(&dialog).to_have_count(0).await
    })
    .await?;
    poll(
        "a folder with the new name exists",
        || async {
            Ok(collections(&t, true)
                .await?
                .iter()
                .any(|collection| collection["name"] == json!(folder_name)))
        },
        |exists| *exists,
    )
    .await?;
    Ok(())
}

async fn nested_new_folder_cancel(t: Ctx) -> Result<(), Error> {
    let page = before_each(&t).await?;
    let key = t.publisher_key().await?;
    let artifact = t
        .publish(
            &key,
            "Dialog picker source",
            &html("Picker source"),
            Some("UI/UX"),
        )
        .await?;
    let artifact_id = id_of(&artifact);
    let (ok, folder) = create_folder(
        &t,
        json!({"org": t.org, "name": "Pending picker selection"}),
    )
    .await?;
    assert!(ok, "{folder}");
    let folder_id = id_of(&folder);
    page.reload().await?;
    let card = page.locator(&format!("#artifact-grid .card[data-id=\"{artifact_id}\"]"));
    expect(&card).to_be_visible().await?;
    card.locator("[data-collection-select]").check().await?;
    page.locator("[data-bulk-collect]").click().await?;
    let picker = page.locator(".collection-picker-dialog");
    expect(&picker).to_be_visible().await?;
    let pending = picker.locator(&format!("input[value=\"{folder_id}\"]"));
    pending.check().await?;
    picker.locator("[data-picker-new]").click().await?;
    let create = page.locator(".collection-create-dialog");
    expect(&create).to_be_visible().await?;
    expect_no_browser_errors(
        &page,
        expect_no_mutation(
            &page,
            create
                .get_by_role(Role::new("button").name("Cancel"))
                .click(),
        ),
    )
    .await?;
    expect(&create).to_have_count(0).await?;
    expect(&picker).to_be_visible().await?;
    expect(&pending).to_be_checked().await?;
    expect(&picker.locator("[data-picker-new]"))
        .to_be_focused()
        .await?;
    expect_no_mutation(
        &page,
        picker
            .get_by_role(Role::new("button").exact_name("Cancel"))
            .click(),
    )
    .await?;
    expect(&picker).to_have_count(0).await
}

async fn edit_cancel(t: Ctx) -> Result<(), Error> {
    edit_exits(t, "Cancel").await
}

async fn edit_close(t: Ctx) -> Result<(), Error> {
    edit_exits(t, "Close").await
}

async fn edit_escape(t: Ctx) -> Result<(), Error> {
    edit_exits(t, "Escape").await
}

/// `edit folder ${close} exits after clearing its required name`.
async fn edit_exits(t: Ctx, close: &str) -> Result<(), Error> {
    let page = before_each(&t).await?;
    let name = format!("Edit cancellation {close}");
    let (ok, folder) = create_folder(&t, json!({"org": t.org, "name": name})).await?;
    assert!(ok, "{folder}");
    let folder_id = id_of(&folder);
    open_edit(&page, &folder_id).await?;
    let dialog = page.get_by_role(Role::new("dialog").exact_name("Edit folder"));
    dialog.get_by_label("Folder name").fill("").await?;
    expect_no_browser_errors(
        &page,
        expect_no_mutation(&page, async {
            if close == "Escape" {
                page.keyboard().press("Escape").await
            } else {
                dialog
                    .get_by_role(Role::new("button").exact_name(close))
                    .click()
                    .await
            }
        }),
    )
    .await?;
    expect(&dialog).to_have_count(0).await?;
    let rows = collections(&t, false).await?;
    let row = rows
        .iter()
        .find(|row| row["id"] == json!(folder_id))
        .expect("the folder is listed");
    assert_eq!(row["name"], json!(name), "{row}");
    Ok(())
}

async fn enter_submits_once(t: Ctx) -> Result<(), Error> {
    let page = before_each(&t).await?;
    // The `request` listener starts here.
    let seen = page.requests().len();
    page.locator("#new-folder").click().await?;
    let dialog = page.get_by_role(Role::new("dialog").exact_name("Create a folder"));
    let name = format!("Keyboard create {}", now_millis());
    dialog.get_by_label("Folder name").fill(&name).await?;
    dialog.get_by_label("Folder name").press("Enter").await?;
    expect(&dialog).to_have_count(0).await?;
    let writes = page.requests()[seen..]
        .iter()
        .filter(|request| request.method == "POST" && pathname(&request.url) == "/collections")
        .count();
    assert_eq!(writes, 1);
    let rows = collections(&t, false).await?;
    assert_eq!(
        rows.iter().filter(|row| row["name"] == json!(name)).count(),
        1
    );
    Ok(())
}

async fn failed_creation_keeps_draft(t: Ctx) -> Result<(), Error> {
    let page = before_each(&t).await?;
    page.locator("#new-folder").click().await?;
    let dialog = page.get_by_role(Role::new("dialog").exact_name("Create a folder"));
    dialog
        .get_by_label("Folder name")
        .fill("Keep this draft")
        .await?;
    page.route_glob("**/collections", |route| async move {
        if route.request().method == "POST" {
            route
                .fulfill(Response::json(
                    503,
                    &json!({"message": "Folder write unavailable"}),
                ))
                .await
        } else {
            route.continue_request().await
        }
    })
    .await?;
    dialog
        .get_by_role(Role::new("button").exact_name("Create folder"))
        .click()
        .await?;
    expect(&dialog.locator(".category-error"))
        .to_have_text("Folder write unavailable")
        .await?;
    expect(&dialog.get_by_label("Folder name"))
        .to_have_value("Keep this draft")
        .await?;
    expect(&dialog.get_by_role(Role::new("button").exact_name("Create folder")))
        .to_be_enabled()
        .await?;
    expect_no_mutation(
        &page,
        dialog
            .get_by_role(Role::new("button").exact_name("Cancel"))
            .click(),
    )
    .await?;
    expect(&dialog).to_have_count(0).await
}

async fn delete_confirmation_cancel(t: Ctx) -> Result<(), Error> {
    delete_confirmation_retains_draft(t, "Cancel").await
}

async fn delete_confirmation_close(t: Ctx) -> Result<(), Error> {
    delete_confirmation_retains_draft(t, "Close").await
}

/// `delete confirmation ${method} retains the edit dialog and its draft`.
async fn delete_confirmation_retains_draft(t: Ctx, method: &str) -> Result<(), Error> {
    let page = before_each(&t).await?;
    let (_, folder) = create_folder(
        &t,
        json!({"org": t.org, "name": format!("Delete confirmation draft {method}")}),
    )
    .await?;
    open_edit(&page, &id_of(&folder)).await?;
    let dialog = page.get_by_role(Role::new("dialog").exact_name("Edit folder"));
    dialog
        .get_by_label("Description")
        .fill("Unsaved description")
        .await?;
    dialog
        .get_by_role(Role::new("button").exact_name("Delete folder"))
        .click()
        .await?;
    let confirmation = page.get_by_role(Role::new("dialog").exact_name("Delete folder"));
    expect(&confirmation).to_be_visible().await?;
    expect_no_mutation(
        &page,
        confirmation
            .get_by_role(Role::new("button").exact_name(method))
            .click(),
    )
    .await?;
    expect(&confirmation).to_have_count(0).await?;
    expect(&dialog).to_be_visible().await?;
    expect(&dialog.get_by_label("Description"))
        .to_have_value("Unsaved description")
        .await?;
    dialog
        .get_by_role(Role::new("button").exact_name("Cancel"))
        .click()
        .await
}

async fn deletion_preserves_artifacts(t: Ctx) -> Result<(), Error> {
    let page = before_each(&t).await?;
    let key = t.publisher_key().await?;
    let artifact = t
        .publish(
            &key,
            "Folder deletion source",
            &html("Deletion source"),
            None,
        )
        .await?;
    let artifact_id = id_of(&artifact);
    let (created_ok, folder) = create_folder(
        &t,
        json!({"org": t.org, "name": "Folder to delete", "artifactIds": [artifact_id]}),
    )
    .await?;
    let (sibling_ok, kept) = create_folder(
        &t,
        json!({"org": t.org, "name": "Folder to keep", "artifactIds": [artifact_id]}),
    )
    .await?;
    assert!(created_ok && sibling_ok, "{folder} {kept}");
    let folder_id = id_of(&folder);
    let kept_id = id_of(&kept);
    open_edit(&page, &folder_id).await?;
    let edit = page.get_by_role(Role::new("dialog").exact_name("Edit folder"));
    edit.get_by_role(Role::new("button").exact_name("Delete folder"))
        .click()
        .await?;
    let confirmation = page.get_by_role(Role::new("dialog").exact_name("Delete folder"));
    expect(&confirmation)
        .to_contain_text(folder["name"].as_str().unwrap_or(""))
        .await?;
    confirmation
        .get_by_role(Role::new("button").exact_name("Delete folder"))
        .click()
        .await?;
    expect(&confirmation).to_have_count(0).await?;
    expect(&page.locator(&format!("[data-collection-peek=\"{folder_id}\"]")))
        .to_have_count(0)
        .await?;
    expect(&page.locator(&format!("[data-collection-peek=\"{kept_id}\"]")))
        .to_be_visible()
        .await?;
    expect(&page.locator(&format!("#artifact-grid .card[data-id=\"{artifact_id}\"]")))
        .to_be_visible()
        .await?;
    let rows = collections(&t, false).await?;
    assert!(
        !rows.iter().any(|row| row["id"] == json!(folder_id)),
        "the deleted folder is gone"
    );
    assert!(
        rows.iter().any(|row| row["id"] == json!(kept_id)
            && strings(&row["artifactIds"]).contains(&artifact_id)),
        "the kept folder still holds the artifact"
    );
    Ok(())
}

async fn escape_cancels_deletion(t: Ctx) -> Result<(), Error> {
    let page = before_each(&t).await?;
    let (_, folder) = create_folder(&t, json!({"org": t.org, "name": "Escape deletion"})).await?;
    let folder_id = id_of(&folder);
    open_edit(&page, &folder_id).await?;
    let edit = page.get_by_role(Role::new("dialog").exact_name("Edit folder"));
    edit.get_by_role(Role::new("button").exact_name("Delete folder"))
        .click()
        .await?;
    let confirmation = page.get_by_role(Role::new("dialog").exact_name("Delete folder"));
    expect_no_mutation(&page, page.keyboard().press("Escape")).await?;
    expect(&confirmation).to_have_count(0).await?;
    expect(&edit).to_be_visible().await?;
    let rows = collections(&t, false).await?;
    assert!(
        rows.iter().any(|row| row["id"] == json!(folder_id)),
        "the folder still exists"
    );
    Ok(())
}

async fn failed_deletion_retryable(t: Ctx) -> Result<(), Error> {
    let page = before_each(&t).await?;
    let (_, folder) = create_folder(&t, json!({"org": t.org, "name": "Failed deletion"})).await?;
    let folder_id = id_of(&folder);
    open_edit(&page, &folder_id).await?;
    let edit = page.get_by_role(Role::new("dialog").exact_name("Edit folder"));
    edit.get_by_role(Role::new("button").exact_name("Delete folder"))
        .click()
        .await?;
    let confirmation = page.get_by_role(Role::new("dialog").exact_name("Delete folder"));
    let glob = format!("**/collections/{folder_id}**");
    page.route_glob(&glob, |route| async move {
        if route.request().method == "DELETE" {
            route
                .fulfill(Response::json(
                    503,
                    &json!({"message": "Folder delete unavailable"}),
                ))
                .await
        } else {
            route.continue_request().await
        }
    })
    .await?;
    confirmation
        .get_by_role(Role::new("button").exact_name("Delete folder"))
        .click()
        .await?;
    expect(&confirmation).to_be_visible().await?;
    expect(&confirmation)
        .to_contain_text("Folder delete unavailable")
        .await?;
    expect(&confirmation.get_by_role(Role::new("button").exact_name("Delete folder")))
        .to_be_enabled()
        .await?;
    page.unroute(&glob).await?;
    confirmation
        .get_by_role(Role::new("button").exact_name("Delete folder"))
        .click()
        .await?;
    expect(&confirmation).to_have_count(0).await?;
    expect(&edit).to_have_count(0).await
}
