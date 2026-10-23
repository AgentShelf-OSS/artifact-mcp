// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
//! `categories`, from `playwright/tests/06-categories.spec.mjs`.

use browser_tests::e2e::{Case, Ctx, encode_component, now_millis};
use browser_tests::{Error, e2e_case, expect};
use serde_json::json;

const SPEC: &str = "06-categories";

pub fn cases() -> Vec<Case> {
    vec![
        e2e_case!(
            SPEC,
            "a category assigned to an artifact appears in Settings",
            assigned_category_in_settings
        ),
        e2e_case!(
            SPEC,
            "create and delete a category in Settings",
            create_and_delete_category
        ),
    ]
}

/// `Date.now().toString().slice(-5)`.
fn last_five_digits() -> String {
    let now = now_millis().to_string();
    now[now.len().saturating_sub(5)..].to_owned()
}

async fn assigned_category_in_settings(t: Ctx) -> Result<(), Error> {
    // Regression guard: the web-UI route used to set the artifact column without registering
    // the category on the org, so it never reached the Settings picker.
    let key = t.publisher_key().await?;
    let page = t.page().await?;
    let artifact = t
        .publish(
            &key,
            &format!("PW Cat {}", t.org),
            "<!doctype html><h1>c</h1>",
            None,
        )
        .await?;
    let category = format!("PWCat-{}", last_five_digits());

    let response = t
        .api(
            "POST",
            &format!("/{}/category", artifact["id"].as_str().unwrap_or("")),
            Some(json!({"category": category})),
        )
        .await?;
    assert_eq!(response.status, 200, "{}", response.text());

    page.goto("/settings").await?;
    page.locator(&format!(r#"[data-org-select="{}"]"#, t.org))
        .click()
        .await?;
    expect(&page.get_by_text(&category)).to_be_visible().await
}

async fn create_and_delete_category(t: Ctx) -> Result<(), Error> {
    let name = format!("PWMade-{}", last_five_digits());
    let path = format!("/settings/orgs/{}/categories", encode_component(&t.org));
    let made = t.api("POST", &path, Some(json!({"name": name}))).await?;
    assert_eq!(made.status, 200, "{}", made.text());
    let gone = t.api("DELETE", &path, Some(json!({"name": name}))).await?;
    assert!([200, 204].contains(&gone.status), "{}", gone.status);
    Ok(())
}
