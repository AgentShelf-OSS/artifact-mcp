// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
//! `feedback`, from `playwright/tests/04-feedback.spec.mjs`.

use browser_tests::e2e::{Case, Ctx};
use browser_tests::{Error, Role, e2e_case, expect, regex};
use serde_json::json;

const SPEC: &str = "04-feedback";

pub fn cases() -> Vec<Case> {
    vec![
        e2e_case!(
            SPEC,
            "add, list, reply, resolve, reopen, delete",
            feedback_lifecycle
        ),
        e2e_case!(SPEC, "empty comment is rejected", empty_comment_rejected),
        e2e_case!(
            SPEC,
            "discussion modes are explicit and two-way fails closed without organization readiness",
            discussion_modes
        ),
    ]
}

async fn feedback_lifecycle(t: Ctx) -> Result<(), Error> {
    let key = t.publisher_key().await?;
    let artifact = t
        .publish(
            &key,
            &format!("PW Fb {}", t.org),
            "<!doctype html><h1>f</h1>",
            None,
        )
        .await?;
    let id = artifact["id"].as_str().unwrap_or("").to_owned();

    let created = t
        .api(
            "POST",
            &format!("/{id}/feedback"),
            Some(json!({"body": "first comment"})),
        )
        .await?;
    assert_eq!(created.status, 201, "{}", created.text());
    let feedback = created.json();
    let feedback_id = feedback["id"].clone();
    assert!(
        !feedback_id.is_null() && feedback_id != json!("") && feedback_id != json!(0),
        "{feedback}"
    );
    let feedback_id = match &feedback_id {
        serde_json::Value::String(text) => text.clone(),
        other => other.to_string(),
    };

    let listed = t.request().get(&format!("/{id}/feedback")).await?;
    assert_eq!(listed.status, 200);
    assert!(listed.text().contains("first comment"));

    let reply = t
        .api(
            "POST",
            &format!("/{id}/feedback"),
            Some(json!({"body": "a reply", "parent_id": feedback["id"]})),
        )
        .await?;
    assert!([200, 201].contains(&reply.status), "{}", reply.status);

    let resolved = t
        .api(
            "POST",
            &format!("/{id}/feedback/{feedback_id}/resolve"),
            Some(json!({"resolved": true})),
        )
        .await?;
    assert_eq!(resolved.status, 200);

    let reopened = t
        .api(
            "POST",
            &format!("/{id}/feedback/{feedback_id}/resolve"),
            Some(json!({"resolved": false})),
        )
        .await?;
    assert_eq!(reopened.status, 200);

    let removed = t
        .api("DELETE", &format!("/{id}/feedback/{feedback_id}"), None)
        .await?;
    assert!([200, 204].contains(&removed.status), "{}", removed.status);
    Ok(())
}

async fn empty_comment_rejected(t: Ctx) -> Result<(), Error> {
    let key = t.publisher_key().await?;
    let artifact = t
        .publish(
            &key,
            &format!("PW FbEmpty {}", t.org),
            "<!doctype html><h1>e</h1>",
            None,
        )
        .await?;
    let response = t
        .api(
            "POST",
            &format!("/{}/feedback", artifact["id"].as_str().unwrap_or("")),
            Some(json!({"body": "   "})),
        )
        .await?;
    assert_eq!(response.status, 400);
    Ok(())
}

async fn discussion_modes(t: Ctx) -> Result<(), Error> {
    let key = t.publisher_key().await?;
    let page = t.page().await?;
    let artifact = t
        .publish(
            &key,
            &format!("PW Discussion modes {}", t.org),
            "<!doctype html><h1>discussion modes</h1>",
            None,
        )
        .await?;
    let id = artifact["id"].as_str().unwrap_or("").to_owned();
    page.goto(&format!("/{id}")).await?;
    page.locator("#vtitle-toggle").click().await?;
    page.get_by_role(Role::new("menuitem").name("Details"))
        .click()
        .await?;

    expect(&page.get_by_role(Role::new("button").name("Keep discussion in Artifact MCP")))
        .to_be_visible()
        .await?;
    expect(&page.get_by_role(Role::new("button").name("Enable two-way Discord sync")))
        .to_be_visible()
        .await?;
    page.get_by_role(Role::new("button").name("Keep discussion in Artifact MCP"))
        .click()
        .await?;
    expect(&page.locator("#vdiscussion-state"))
        .to_have_text("Artifact MCP only")
        .await?;
    expect(&page.get_by_role(Role::new("button").name("Use organization default")))
        .to_be_visible()
        .await?;

    page.get_by_role(Role::new("button").name("Enable two-way Discord sync"))
        .click()
        .await?;
    let discussion_status = page.locator("#vdiscussion-status");
    expect(&discussion_status)
        .to_have_class(regex("error", ""))
        .await?;
    expect(&discussion_status)
        .to_contain_text(regex(
            "organization Discord credential.*outbound threading policy.*ready",
            "i",
        ))
        .await?;

    let local = t
        .api(
            "POST",
            &format!("/{id}/feedback"),
            Some(json!({"body": "local feedback survives unavailable Discord"})),
        )
        .await?;
    assert_eq!(local.status, 201, "{}", local.text());
    Ok(())
}
