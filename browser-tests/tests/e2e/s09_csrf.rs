// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
//! `portal request authenticity`, from `playwright/tests/09-csrf.spec.mjs`.

use browser_tests::e2e::{Case, Ctx, now_millis};
use browser_tests::{Error, Role, e2e_case, expect};
use serde_json::json;

const SPEC: &str = "09-csrf";

pub fn cases() -> Vec<Case> {
    vec![
        e2e_case!(
            SPEC,
            "a real portal action adds the mutation header and succeeds",
            portal_action_header
        ),
        e2e_case!(
            SPEC,
            "a sandboxed artifact cannot revoke a publisher key with a bodyless cross-site POST",
            sandboxed_cross_site_post
        ),
        e2e_case!(
            SPEC,
            "the discussion control uses the same CSRF mutation header",
            discussion_mutation_header
        ),
    ]
}

async fn portal_action_header(t: Ctx) -> Result<(), Error> {
    let key = t.publisher_key().await?;
    let page = t.page().await?;
    let artifact = t
        .publish(
            &key,
            &format!("PW CSRF UI {}", t.org),
            "<!doctype html><h1>trusted shell action</h1>",
            None,
        )
        .await?;
    let id = artifact["id"].as_str().unwrap_or("").to_owned();

    page.goto(&format!("/{id}")).await?;
    let favorite = page.locator(".vreact.fav");
    let react = format!("/{id}/react");
    let response = page.wait_for_response(move |candidate| {
        candidate.url.contains(&react) && candidate.request.method == "POST"
    });
    favorite.click().await?;
    let response = response.await?;

    assert_eq!(response.status, 200);
    assert_eq!(response.request.header("x-artifact-mutation"), Some("1"));
    expect(&favorite)
        .to_have_attribute("aria-pressed", "true")
        .await
}

async fn sandboxed_cross_site_post(t: Ctx) -> Result<(), Error> {
    let key = t.publisher_key().await?;
    let page = t.page().await?;
    let now = now_millis().to_string();
    let client_id = format!("pwcsrf-{}", &now[now.len().saturating_sub(7)..]);
    let issued = t
        .api(
            "POST",
            "/settings/keys",
            Some(json!({"clientId": client_id, "org": t.org, "label": "CSRF target", "role": "author"})),
        )
        .await?;
    assert_eq!(issued.status, 200, "{}", issued.text());
    let target_key = issued.json();
    let target_secret = target_key["secret"].as_str().unwrap_or("").to_owned();
    assert!(!target_secret.is_empty(), "{target_key}");

    // The raw-artifact CSP correctly blocks `fetch` with `connect-src 'none'`, so exercise the
    // browser primitive that remains intentionally available to artifacts: a bodyless form
    // POST. The iframe is opaque-origin because its sandbox omits `allow-same-origin`.
    let attacker = t
        .publish(
            &key,
            &format!("PW CSRF attacker {}", t.org),
            &format!(
                r#"<!doctype html><h1>untrusted</h1>
        <form id="csrf-form" action="/settings/keys/{client_id}/revoke" method="post"></form>
        <script>document.getElementById('csrf-form').submit();</script>"#
            ),
            None,
        )
        .await?;

    let revoke = format!("/settings/keys/{client_id}/revoke");
    let forged_response = page.wait_for_response(move |candidate| {
        candidate.url.contains(&revoke) && candidate.request.method == "POST"
    });
    page.goto(&format!("/{}", attacker["id"].as_str().unwrap_or("")))
        .await?;
    let frame = page.locator("#vframe");
    expect(&frame).to_be_visible().await?;
    let sandbox = frame.get_attribute("sandbox").await?.unwrap_or_default();
    assert!(!sandbox.contains("allow-same-origin"), "{sandbox}");
    let response = forged_response.await?;
    let request = &response.request;
    assert_eq!(response.status, 403);
    assert_eq!(request.header("x-artifact-mutation"), None);
    // Chromium normally labels this opaque sandbox request with Origin: null. The hard
    // assertion above is portable; this accepts a browser omitting it while still rejecting a
    // forged origin.
    if let Some(origin) = request.header("origin") {
        assert_eq!(origin, "null");
    }
    if let Some(site) = request.header("sec-fetch-site") {
        assert_ne!(site, "same-origin");
    }

    // A successful publish independently proves the target key was not revoked.
    let result = t
        .publish(
            &target_secret,
            &format!("PW CSRF target still active {}", t.org),
            "<!doctype html><h1>key remains active</h1>",
            None,
        )
        .await?;
    assert!(
        result["id"].as_str().is_some_and(|id| !id.is_empty()),
        "{result}"
    );
    Ok(())
}

async fn discussion_mutation_header(t: Ctx) -> Result<(), Error> {
    let key = t.publisher_key().await?;
    let page = t.page().await?;
    let artifact = t
        .publish(
            &key,
            &format!("PW CSRF discussion {}", t.org),
            "<!doctype html><h1>discussion action</h1>",
            None,
        )
        .await?;
    let id = artifact["id"].as_str().unwrap_or("").to_owned();

    page.goto(&format!("/{id}")).await?;
    page.locator("#vtitle-toggle").click().await?;
    page.get_by_role(Role::new("menuitem").name("Details"))
        .click()
        .await?;
    let path = format!("/{id}/discussion/override");
    let response = page.wait_for_response(move |candidate| {
        candidate.url.contains(&path) && candidate.request.method == "PUT"
    });
    page.get_by_role(Role::new("button").name("Keep discussion in Artifact MCP"))
        .click()
        .await?;
    let response = response.await?;
    assert_eq!(response.status, 200);
    assert_eq!(response.request.header("x-artifact-mutation"), Some("1"));
    Ok(())
}
