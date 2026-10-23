// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
//! Shared in-app confirmations, from `playwright/tests/17-shared-confirmations.spec.mjs`.

use browser_tests::e2e::{Case, Ctx, encode_component, now_millis};
use browser_tests::{DialogAction, Error, Role, e2e_case, expect, poll, regex};
use serde_json::json;

const SPEC: &str = "17-shared-confirmations";

pub fn cases() -> Vec<Case> {
    vec![
        e2e_case!(
            SPEC,
            "organization deletion Cancel preserves the registry",
            org_deletion_cancel
        ),
        e2e_case!(
            SPEC,
            "organization deletion Close preserves the registry",
            org_deletion_close
        ),
        e2e_case!(
            SPEC,
            "organization deletion Escape preserves the registry",
            org_deletion_escape
        ),
        e2e_case!(
            SPEC,
            "publisher-key revocation uses the in-app confirmation and preserves CSRF protection",
            key_revocation
        ),
        e2e_case!(
            SPEC,
            "restoring a revision confirms its effect and keeps revision history",
            restore_revision
        ),
    ]
}

async fn org_deletion_cancel(t: Ctx) -> Result<(), Error> {
    org_deletion(t, "Cancel").await
}

async fn org_deletion_close(t: Ctx) -> Result<(), Error> {
    org_deletion(t, "Close").await
}

async fn org_deletion_escape(t: Ctx) -> Result<(), Error> {
    org_deletion(t, "Escape").await
}

/// `organization deletion ${dismissal} preserves the registry`.
async fn org_deletion(t: Ctx, dismissal: &str) -> Result<(), Error> {
    let page = t.page().await?;
    let org = t.org.clone();
    // `page.on("dialog")` dismisses native dialogs; `page.dialogs()` lists them.
    page.on_dialog(|_| DialogAction::Dismiss);
    page.goto(&format!(
        "/settings#tab=organizations&org={}",
        encode_component(&org)
    ))
    .await?;
    let trigger = page.locator(&format!("[data-org-detail=\"{org}\"] .org-del"));
    trigger.click().await?;
    let dialog = page.get_by_role(Role::new("dialog").exact_name("Delete organization"));
    expect(&dialog).to_contain_text(org.as_str()).await?;
    expect(&dialog.get_by_role(Role::new("button").name("Cancel")))
        .to_be_focused()
        .await?;
    // The `request` listener starts here.
    let seen = page.requests().len();
    if dismissal == "Escape" {
        page.keyboard().press("Escape").await?;
    } else {
        dialog
            .get_by_role(Role::new("button").exact_name(dismissal))
            .click()
            .await?;
    }
    expect(&dialog).to_have_count(0).await?;
    expect(&trigger).to_be_focused().await?;
    let writes: Vec<String> = page.requests()[seen..]
        .iter()
        .filter(|request| request.method == "DELETE")
        .map(|request| request.url.clone())
        .collect();
    assert_eq!(writes, Vec::<String>::new());
    let native: Vec<String> = page
        .dialogs()
        .into_iter()
        .map(|dialog| dialog.kind)
        .collect();
    assert_eq!(native, Vec::<String>::new());
    Ok(())
}

async fn key_revocation(t: Ctx) -> Result<(), Error> {
    let page = t.page().await?;
    let client_id = format!("confirm-key-{}", now_millis());
    let response = t
        .api(
            "POST",
            "/settings/keys",
            Some(json!({"clientId": client_id, "org": t.org, "label": "Confirmation QA"})),
        )
        .await?;
    assert!(response.ok(), "{} {}", response.status, response.text());
    page.goto("/settings#tab=keys").await?;
    let row = page.locator(&format!("tr[data-id=\"{client_id}\"]"));
    row.locator(".revoke").click().await?;
    let dialog = page.get_by_role(Role::new("dialog").exact_name("Revoke publisher key"));
    expect(&dialog).to_contain_text(client_id.as_str()).await?;
    let suffix = format!("/settings/keys/{client_id}/revoke");
    let mutation = page.wait_for_request(move |request| {
        request.url.ends_with(&suffix) && request.method == "POST"
    });
    dialog
        .get_by_role(Role::new("button").exact_name("Revoke key"))
        .click()
        .await?;
    let mutation = mutation.await?;
    assert_eq!(mutation.header("x-artifact-mutation"), Some("1"));
    expect(&row).to_have_class(regex("revoked", "")).await?;
    expect(&dialog).to_have_count(0).await
}

async fn restore_revision(t: Ctx) -> Result<(), Error> {
    let key = t.publisher_key().await?;
    let page = t.page().await?;
    let artifact = t
        .publish(
            &key,
            "Restore confirmation",
            "<!doctype html><h1>Version one</h1>",
            None,
        )
        .await?;
    let id = artifact["id"].as_str().unwrap_or("").to_owned();
    t.mcp_call(
        &key,
        "update_artifact",
        json!({"id": id, "html": "<!doctype html><h1>Version two</h1>"}),
    )
    .await?;
    page.goto(&format!("/{id}")).await?;
    page.locator("#vtitle-toggle").click().await?;
    page.locator("[data-inspector-open=\"history\"]")
        .click()
        .await?;
    page.locator(".vh-restore[data-rev=\"1\"]").click().await?;
    let dialog = page.get_by_role(Role::new("dialog").exact_name("Restore revision"));
    expect(&dialog)
        .to_contain_text("All existing revisions will remain")
        .await?;
    dialog
        .get_by_role(Role::new("button").exact_name("Cancel"))
        .click()
        .await?;
    let history = format!("/{id}/history");
    let before = t.request().get(&history).await?.json();
    assert_eq!(before["current"], json!(2), "{before}");
    page.locator(".vh-restore[data-rev=\"1\"]").click().await?;
    dialog
        .get_by_role(Role::new("button").exact_name("Restore revision"))
        .click()
        .await?;
    poll(
        "the current revision",
        || async { Ok(t.request().get(&history).await?.json()["current"].clone()) },
        |current| *current == json!(3),
    )
    .await?;
    Ok(())
}
