// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
//! `reactions`, from `playwright/tests/03-reactions.spec.mjs`.

use browser_tests::e2e::{Case, Ctx};
use browser_tests::{Error, e2e_case, expect};

const SPEC: &str = "03-reactions";

pub fn cases() -> Vec<Case> {
    vec![
        e2e_case!(
            SPEC,
            "heart fires a request and flips pressed state",
            heart_fires_request
        ),
        e2e_case!(SPEC, "upvote and downvote toggle", votes_toggle),
        e2e_case!(SPEC, "reaction persists across reload", reaction_persists),
    ]
}

async fn heart_fires_request(t: Ctx) -> Result<(), Error> {
    // Regression guard for the truncated shell.js: the script was served faithfully but no
    // listener attached, so clicking did nothing. Assert the click has an EFFECT.
    let key = t.publisher_key().await?;
    let page = t.page().await?;
    let artifact = t
        .publish(
            &key,
            &format!("PW React {}", t.org),
            "<!doctype html><h1>r</h1>",
            None,
        )
        .await?;
    let id = artifact["id"].as_str().unwrap_or("").to_owned();
    page.goto(&format!("/{id}")).await?;
    let fav = page.locator(".vreact.fav");
    expect(&fav)
        .to_have_attribute("aria-pressed", "false")
        .await?;
    let react = format!("/{id}/react");
    let response = page.wait_for_response(move |response| {
        response.url.contains(&react) && response.request.method == "POST"
    });
    fav.click().await?;
    assert_eq!(response.await?.status, 200);
    expect(&fav).to_have_attribute("aria-pressed", "true").await
}

async fn votes_toggle(t: Ctx) -> Result<(), Error> {
    let key = t.publisher_key().await?;
    let page = t.page().await?;
    let artifact = t
        .publish(
            &key,
            &format!("PW Vote {}", t.org),
            "<!doctype html><h1>v</h1>",
            None,
        )
        .await?;
    let id = artifact["id"].as_str().unwrap_or("").to_owned();
    page.goto(&format!("/{id}")).await?;
    page.locator("#vtitle-toggle").click().await?;
    let react = format!("/{id}/react");
    let up = page.locator(".vreact.up");
    let path = react.clone();
    let response = page.wait_for_response(move |response| response.url.contains(&path));
    up.click().await?;
    response.await?;
    expect(&up)
        .to_have_attribute("aria-checked", "true")
        .await?;
    let down = page.locator(".vreact.down");
    let path = react.clone();
    let response = page.wait_for_response(move |response| response.url.contains(&path));
    down.click().await?;
    response.await?;
    expect(&down)
        .to_have_attribute("aria-checked", "true")
        .await?;
    expect(&up).to_have_attribute("aria-checked", "false").await
}

async fn reaction_persists(t: Ctx) -> Result<(), Error> {
    let key = t.publisher_key().await?;
    let page = t.page().await?;
    let artifact = t
        .publish(
            &key,
            &format!("PW Persist {}", t.org),
            "<!doctype html><h1>p</h1>",
            None,
        )
        .await?;
    page.goto(&format!("/{}", artifact["id"].as_str().unwrap_or("")))
        .await?;
    let fav = page.locator(".vreact.fav");
    let response = page.wait_for_response(|response| response.url.contains("/react"));
    fav.click().await?;
    response.await?;
    page.reload().await?;
    expect(&page.locator(".vreact.fav"))
        .to_have_attribute("aria-pressed", "true")
        .await
}
