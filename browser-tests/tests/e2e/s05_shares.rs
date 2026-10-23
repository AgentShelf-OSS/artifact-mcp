// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
//! `sharing`, from `playwright/tests/05-shares.spec.mjs`.

use browser_tests::e2e::{Case, Ctx, Identity};
use browser_tests::{Error, e2e_case, expect};
use serde_json::{Value, json};

const SPEC: &str = "05-shares";

pub fn cases() -> Vec<Case> {
    vec![
        e2e_case!(
            SPEC,
            "create, list, use publicly, revoke, then 404",
            share_lifecycle
        ),
        e2e_case!(SPEC, "rejects an invalid expiry", invalid_expiry),
        e2e_case!(
            SPEC,
            "public shares preserve HTML and do not enable state for unsigned or signed-in visitors",
            public_share_html
        ),
    ]
}

async fn share_lifecycle(t: Ctx) -> Result<(), Error> {
    let key = t.publisher_key().await?;
    let artifact = t
        .publish(
            &key,
            &format!("PW Share {}", t.org),
            "<!doctype html><h1>s</h1>",
            None,
        )
        .await?;
    let id = artifact["id"].as_str().unwrap_or("").to_owned();

    let created = t
        .api(
            "POST",
            &format!("/{id}/share"),
            Some(json!({"expires": "never"})),
        )
        .await?;
    assert_eq!(created.status, 200, "{}", created.text());
    let share = created.json();
    let token = share["token"]
        .as_str()
        .or_else(|| share["share"]["token"].as_str())
        .unwrap_or("")
        .to_owned();
    assert!(!token.is_empty(), "{share}");

    let listed = t.request().get(&format!("/{id}/shares")).await?;
    assert_eq!(listed.status, 200);

    // The public page must work with NO identity at all.
    let anonymous = t.request_as(&Identity::Anonymous);
    let public = anonymous
        .get(&format!("{}/s/{token}", t.base_url()))
        .await?;
    assert_eq!(public.status, 200, "public share must load unauthenticated");
    assert!(
        public
            .header("cache-control")
            .unwrap_or("")
            .contains("no-store"),
        "{:?}",
        public.header("cache-control")
    );

    let revoked = t
        .api("DELETE", &format!("/{id}/shares/{token}"), None)
        .await?;
    assert!([200, 204].contains(&revoked.status), "{}", revoked.status);

    let after = anonymous
        .get(&format!("{}/s/{token}", t.base_url()))
        .await?;
    assert_eq!(after.status, 404, "revoked share must 404");

    // An invented token must be indistinguishable from a revoked one.
    let bogus = anonymous
        .get(&format!("{}/s/{}", t.base_url(), "z".repeat(24)))
        .await?;
    assert_eq!(bogus.status, 404);
    Ok(())
}

async fn invalid_expiry(t: Ctx) -> Result<(), Error> {
    let key = t.publisher_key().await?;
    let artifact = t
        .publish(
            &key,
            &format!("PW ShareBad {}", t.org),
            "<!doctype html><h1>x</h1>",
            None,
        )
        .await?;
    let response = t
        .api(
            "POST",
            &format!("/{}/share", artifact["id"].as_str().unwrap_or("")),
            Some(json!({"expires": "not-a-date"})),
        )
        .await?;
    assert_eq!(response.status, 400);
    Ok(())
}

async fn public_share_html(t: Ctx) -> Result<(), Error> {
    let key = t.publisher_key().await?;
    let page = t.page().await?;
    let html = "<!doctype html><h1>share</h1>";
    let artifact = t
        .publish(&key, &format!("PW Share State {}", t.org), html, None)
        .await?;
    let id = artifact["id"].as_str().unwrap_or("").to_owned();
    let created = t
        .api(
            "POST",
            &format!("/{id}/share"),
            Some(json!({"expires": "never"})),
        )
        .await?;
    assert_eq!(created.status, 200);
    let token = created.json()["token"].as_str().unwrap_or("").to_owned();
    let anonymous_page = t.page_as(Identity::Anonymous).await?;
    for visitor in [&page, &anonymous_page] {
        let response = visitor
            .goto(&format!("{}/s/{token}", t.base_url()))
            .await?
            .expect("the share has a response");
        assert_eq!(response.status, 200);
        assert_eq!(visitor.response_text(&response).await?, html);
        // If public delivery ever gains a shell, it must keep state disabled.
        expect(&visitor.locator(r#"#shell-config[data-state-enabled="1"]"#))
            .to_have_count(0)
            .await?;
    }
    let state = t
        .request_as(&Identity::Anonymous)
        .get(&format!("{}/{id}/state/private", t.base_url()))
        .await?;
    assert_eq!(state.status, 404);
    let updated = t
        .mcp_call(
            &key,
            "update_artifact",
            json!({"id": id, "html": "<h1>updated</h1>"}),
        )
        .await?;
    let reply = updated.json();
    assert!(reply.get("result").is_some(), "{reply}");
    assert_ne!(reply["result"]["isError"], Value::Bool(true));
    let history = page
        .goto(&format!("{}/raw/{id}/rev/1", t.base_url()))
        .await?
        .expect("the revision has a response");
    assert_eq!(history.status, 200);
    assert_eq!(page.response_text(&history).await?, html);
    Ok(())
}
