// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
//! `visibility & tenancy`, from `playwright/tests/07-tenancy.spec.mjs`.

use browser_tests::e2e::{Case, Ctx, Identity, encode_component, unique_suffix};
use browser_tests::http::Body;
use browser_tests::{Error, Locator, Page, Role, e2e_case, expect, regex};
use serde_json::json;

const SPEC: &str = "07-tenancy";

pub fn cases() -> Vec<Case> {
    vec![
        e2e_case!(SPEC, "hide and unhide an artifact", hide_and_unhide),
        e2e_case!(
            SPEC,
            "a nonexistent artifact and a concealed one answer identically",
            nonexistent_and_concealed
        ),
        e2e_case!(SPEC, "delete removes the artifact", delete_removes),
        e2e_case!(
            SPEC,
            "gallery eye and delete controls are admin-or-recorded-owner only",
            owner_controls
        ),
    ]
}

async fn hide_and_unhide(t: Ctx) -> Result<(), Error> {
    let key = t.publisher_key().await?;
    let artifact = t
        .publish(
            &key,
            &format!("PW Vis {}", t.org),
            "<!doctype html><h1>v</h1>",
            None,
        )
        .await?;
    let path = format!("/{}/visibility", artifact["id"].as_str().unwrap_or(""));
    let hidden = t.api("POST", &path, Some(json!({"hidden": true}))).await?;
    assert_eq!(hidden.status, 200, "{}", hidden.text());
    let shown = t.api("POST", &path, Some(json!({"hidden": false}))).await?;
    assert_eq!(shown.status, 200);
    Ok(())
}

async fn nonexistent_and_concealed(t: Ctx) -> Result<(), Error> {
    // invariant 3: existence must not leak. Both must be the same 404 shape.
    let missing = t.request().get("/zzzznotreal99").await?;
    assert_eq!(missing.status, 404);
    let missing_api = t.request().get("/zzzznotreal99/shares").await?;
    assert_eq!(missing_api.status, 404);
    assert!(
        missing_api.text().contains("Not found"),
        "{}",
        missing_api.text()
    );
    Ok(())
}

async fn delete_removes(t: Ctx) -> Result<(), Error> {
    let key = t.publisher_key().await?;
    let artifact = t
        .publish(
            &key,
            &format!("PW Del {}", t.org),
            "<!doctype html><h1>d</h1>",
            None,
        )
        .await?;
    let id = artifact["id"].as_str().unwrap_or("").to_owned();
    let deleted = t.api("DELETE", &format!("/{id}"), None).await?;
    assert!([200, 204].contains(&deleted.status), "{}", deleted.status);
    let after = t.request().get(&format!("/{id}")).await?;
    assert_eq!(after.status, 404);
    Ok(())
}

/// `memberPage(email)`: a new context and page for `email`, on the gallery.
async fn member_page(t: &Ctx, email: &str) -> Result<Page, Error> {
    let page = t.page_as(Identity::email(email)).await?;
    page.goto(&format!("{}/", t.base_url())).await?;
    Ok(page)
}

fn card<'p>(page: &'p Page, id: &str) -> Locator<'p> {
    page.locator(&format!(r#"[data-id="{id}"]"#))
}

async fn owner_controls(t: Ctx) -> Result<(), Error> {
    let org = &t.org;
    let suffix = unique_suffix();
    let client_id = format!("pwowned-{}-{suffix}", t.run_id);
    let owner_email = format!("owner-{suffix}@example.test");
    let member_email = format!("member-{suffix}@example.test");

    for email in [&owner_email, &member_email] {
        let member = t
            .api(
                "POST",
                &format!("/settings/orgs/{}/emails", encode_component(org)),
                Some(json!({"email": email})),
            )
            .await?;
        assert_eq!(member.status, 200, "{}", member.text());
    }

    let created = t
        .api(
            "POST",
            "/settings/keys",
            Some(json!({"clientId": client_id, "org": org, "label": "owner-browser-qa"})),
        )
        .await?;
    assert_eq!(created.status, 200, "{}", created.text());
    let created_body = created.json();
    let key = created_body["secret"]
        .as_str()
        .or_else(|| created_body["key"].as_str())
        .unwrap_or("")
        .to_owned();
    assert!(!key.is_empty(), "{created_body}");

    let assigned = t
        .api(
            "POST",
            &format!("/settings/keys/{}/owner", encode_component(&client_id)),
            Some(json!({"ownerEmail": owner_email})),
        )
        .await?;
    assert_eq!(assigned.status, 200, "{}", assigned.text());

    let artifact = t
        .publish(
            &key,
            &format!("PW owner controls {suffix}"),
            "<!doctype html><h1>owner controls</h1>",
            None,
        )
        .await?;
    let id = artifact["id"].as_str().unwrap_or("").to_owned();
    let visibility_path = format!("/{id}/visibility");

    let owner = member_page(&t, &owner_email).await?;
    let owner_card = card(&owner, &id);
    expect(&owner_card)
        .to_have_attribute("data-owned", "1")
        .await?;
    expect(&owner_card.locator(r#"[data-action="visibility"]"#))
        .to_be_visible()
        .await?;
    owner_card
        .locator(r#"[data-action="more"]"#)
        .click()
        .await?;
    expect(&owner_card.get_by_role(Role::new("button").name("Delete artifact")))
        .to_be_visible()
        .await?;
    let eye = owner_card.locator(r#"[data-action="visibility"]"#);
    let path = visibility_path.clone();
    let hidden_request = owner.wait_for_response(move |response| {
        response.url.ends_with(&path) && response.request.method == "POST"
    });
    eye.click().await?;
    assert_eq!(hidden_request.await?.status, 200);
    expect(&eye)
        .to_have_attribute("aria-label", regex("Show .* in the gallery", ""))
        .await?;
    expect(&eye)
        .to_have_attribute("title", "Show in gallery")
        .await?;
    expect(&eye.locator("svg path").first())
        .to_have_attribute("d", "m3 3 18 18")
        .await?;
    expect(&owner.locator("#toast"))
        .to_have_text("Artifact hidden from Gallery")
        .await?;
    owner.reload().await?;
    expect(&owner_card).to_be_visible().await?;
    expect(&owner_card.locator(r#"[data-action="visibility"]"#))
        .to_have_attribute("aria-label", regex("Show .* in the gallery", ""))
        .await?;
    let hidden_member = member_page(&t, &member_email).await?;
    expect(&card(&hidden_member, &id)).to_have_count(0).await?;
    hidden_member.close().await;
    let show_eye = owner_card.locator(r#"[data-action="visibility"]"#);
    let path = visibility_path.clone();
    let shown_request = owner.wait_for_response(move |response| {
        response.url.ends_with(&path) && response.request.method == "POST"
    });
    show_eye.click().await?;
    assert_eq!(shown_request.await?.status, 200);
    expect(&show_eye)
        .to_have_attribute("title", "Hide from gallery")
        .await?;
    expect(&show_eye.locator("svg path").first())
        .to_have_attribute("d", regex(r"M2\.062 12\.348", ""))
        .await?;
    expect(&owner.locator("#toast"))
        .to_have_text("Artifact shown in Gallery")
        .await?;
    owner.reload().await?;
    expect(&owner_card.locator(r#"[data-action="visibility"]"#))
        .to_have_attribute("aria-label", regex("Hide .* in the gallery", ""))
        .await?;

    let non_owner = member_page(&t, &member_email).await?;
    let non_owner_card = card(&non_owner, &id);
    expect(&non_owner_card)
        .to_have_attribute("data-owned", "0")
        .await?;
    expect(&non_owner_card.locator(r#"[data-action="visibility"]"#))
        .to_have_count(0)
        .await?;
    non_owner_card
        .locator(r#"[data-action="more"]"#)
        .click()
        .await?;
    expect(&non_owner_card.get_by_role(Role::new("button").name("Delete artifact")))
        .to_have_count(0)
        .await?;
    let denied = t
        .request_as(&Identity::email(&member_email))
        .send(
            "POST",
            &format!("{}{visibility_path}", t.base_url()),
            Some(Body::Json(json!({"hidden": true}))),
            &[
                ("content-type", "application/json"),
                ("X-Artifact-Mutation", "1"),
                ("Sec-Fetch-Site", "same-origin"),
            ],
        )
        .await?;
    assert_eq!(denied.status, 403);
    non_owner.close().await;
    owner.close().await;

    let admin_headers = t.admin_headers();
    let admin = t.page_as(Identity::Headers(admin_headers)).await?;
    admin.goto(&format!("{}/", t.base_url())).await?;
    let admin_card = card(&admin, &id);
    expect(&admin_card.locator(r#"[data-action="visibility"]"#))
        .to_be_visible()
        .await?;
    admin_card
        .locator(r#"[data-action="more"]"#)
        .click()
        .await?;
    expect(&admin_card.get_by_role(Role::new("button").name("Delete artifact")))
        .to_be_visible()
        .await
}
