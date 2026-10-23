// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
//! MCP folders, from `playwright/tests/15-mcp-collections.spec.mjs`.

use browser_tests::e2e::{Case, Ctx};
use browser_tests::{Error, e2e_case, expect, poll};
use serde_json::{Value, json};

const SPEC: &str = "15-mcp-collections";

pub fn cases() -> Vec<Case> {
    vec![e2e_case!(
        SPEC,
        "MCP folders share all library views and preserve artifacts and preferences",
        folders_share_views
    )]
}

/// Call an MCP tool and return its `structuredContent`; the call must succeed.
async fn call(t: &Ctx, key: &str, name: &str, arguments: Value) -> Result<Value, Error> {
    let response = t.mcp_call(key, name, arguments).await?;
    assert!(
        response.ok(),
        "{name}: {} {}",
        response.status,
        response.text()
    );
    let body = response.json();
    assert!(body.get("error").is_none(), "{name}: {body}");
    assert_ne!(
        body.pointer("/result/isError"),
        Some(&json!(true)),
        "{name}: {body}"
    );
    Ok(body["result"]["structuredContent"].clone())
}

/// The ids in a list of ids or of objects with an `id`.
fn ids(list: &Value) -> Vec<String> {
    list.as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| item.as_str().or_else(|| item["id"].as_str()))
        .map(str::to_owned)
        .collect()
}

/// `expect(have).toEqual(expect.arrayContaining(wanted))`.
fn contains_all(have: &[String], wanted: &[String]) -> bool {
    wanted.iter().all(|id| have.contains(id))
}

fn has_name(names: &Value, name: &str) -> bool {
    names
        .as_array()
        .is_some_and(|names| names.contains(&json!(name)))
}

const NAMES: &str = "window.ArtifactCollections?.getState()?.collections?.map((c) => c.name) || []";

async fn folders_share_views(t: Ctx) -> Result<(), Error> {
    let key = t.publisher_key().await?;
    let page = t.page().await?;
    let org = t.org.clone();
    let mut artifacts = Vec::new();
    for title in ["MCP Alpha", "MCP Beta"] {
        let artifact = t
            .publish(
                &key,
                title,
                &format!("<!doctype html><h1>{title}</h1>"),
                Some("MCP QA"),
            )
            .await?;
        artifacts.push(artifact["id"].as_str().unwrap_or("").to_owned());
    }
    let prefs = t
        .api("GET", &format!("/gallery/preferences?org={org}"), None)
        .await?
        .json();
    let created = call(
        &t,
        &key,
        "create_collection",
        json!({"name": "MCP collection QA", "artifact_ids": [artifacts[0]], "cover_artifact_id": artifacts[0]}),
    )
    .await?;
    let id = created["collection"]["id"]
        .as_str()
        .unwrap_or("")
        .to_owned();
    call(
        &t,
        &key,
        "add_artifacts_to_collection",
        json!({"id": id, "artifact_ids": [artifacts[1]]}),
    )
    .await?;
    let collection = call(&t, &key, "get_collection", json!({"id": id})).await?;
    let listed = ids(&collection["artifacts"]);
    assert!(contains_all(&listed, &artifacts), "{listed:?}");
    let prefs_after = t
        .api("GET", &format!("/gallery/preferences?org={org}"), None)
        .await?
        .json();
    assert_eq!(prefs_after, prefs);
    for view in ["reel", "sheets", "ribbons"] {
        page.goto(&format!("/?org={org}&libraryView={view}"))
            .await?;
        poll(
            "the collection names contain \"MCP collection QA\"",
            || async { page.evaluate(NAMES).await },
            |names| has_name(names, "MCP collection QA"),
        )
        .await?;
        let state = page
            .call(
                "(id) => window.ArtifactCollections.getState().collections.find((c) => c.id === id)",
                &[json!(id)],
            )
            .await?;
        let state_ids = ids(&state["artifactIds"]);
        assert!(contains_all(&state_ids, &artifacts), "{view}: {state}");
        expect(&page.locator("#collection-surface"))
            .to_contain_text("MCP collection QA")
            .await?;
    }
    call(
        &t,
        &key,
        "remove_artifacts_from_collection",
        json!({"id": id, "artifact_ids": [artifacts[0]]}),
    )
    .await?;
    page.reload().await?;
    poll(
        "the folder lists only the second artifact",
        || async {
            page.call(
                "(id) => window.ArtifactCollections?.getState()?.collections?.find((c) => c.id === id)?.artifactIds || []",
                &[json!(id)],
            )
            .await
        },
        |value| *value == json!([artifacts[1]]),
    )
    .await?;
    call(&t, &key, "delete_collection", json!({"id": id})).await?;
    page.reload().await?;
    poll(
        "the collection names do not contain \"MCP collection QA\"",
        || async { page.evaluate(NAMES).await },
        |names| !has_name(names, "MCP collection QA"),
    )
    .await?;
    for artifact in &artifacts {
        let read = call(&t, &key, "read_artifact", json!({"id": artifact})).await?;
        assert!(read.to_string().contains(artifact.as_str()), "{read}");
    }
    assert_eq!(page.page_errors(), Vec::<String>::new());
    Ok(())
}
