// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
//! Registered MCP folder tools with real migrated collection persistence.

use std::{collections::BTreeSet, path::PathBuf, sync::Arc};

use artifact_mcp::{
    AppDeps,
    config::AppConfig,
    error::{AppError, McpError},
    mcp::{dispatch::dispatch, protocol::OrderedJson},
    model::{ClientId, OrgId, PublisherIdentity},
    persistence::db::Database,
};
use serde_json::{Value, json};

struct TempDir(PathBuf);
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn fixture() -> (TempDir, AppDeps) {
    let dir = TempDir(std::env::temp_dir().join(format!("mcp-folders-86-{}", nanoid::nanoid!(12))));
    std::fs::create_dir_all(&dir.0).unwrap();
    let pool = Database::open_at(&dir.0).unwrap();
    pool.get()
        .unwrap()
        .execute("INSERT INTO orgs(name) VALUES ('acme')", [])
        .unwrap();
    let mut config = AppConfig::defaults();
    config.data_dir = dir.0.clone();
    config.oauth.issuer = "https://issuer.example.test".into();
    let mut deps = super::u18_admin_routes::collection_test_deps(config);
    deps.data = Arc::new(artifact_mcp::data::DataBroker::empty().with_pool(pool));
    (dir, deps)
}

fn oauth(scopes: &[&str]) -> PublisherIdentity {
    PublisherIdentity {
        client_id: ClientId::from("scope-client"),
        org: OrgId::from("acme"),
        label: "synthetic test credential".into(),
        role: "collaborator".into(),
        scopes: Some(
            scopes
                .iter()
                .map(|s| (*s).to_owned())
                .collect::<BTreeSet<_>>(),
        ),
    }
}

async fn call(
    deps: &AppDeps,
    auth: &PublisherIdentity,
    tool: &str,
    args: Value,
) -> Result<Value, McpError> {
    let message: OrderedJson = serde_json::from_value(json!({
        "jsonrpc":"2.0", "id":"caller-request-id", "method":"tools/call",
        "params":{"name":tool,"arguments":args}
    }))
    .unwrap();
    dispatch(&message, auth, deps).await
}

#[tokio::test]
async fn every_registered_folder_tool_requires_its_exact_oauth_scope() {
    let (_dir, deps) = fixture();
    let owner = oauth(&["artifacts:publish"]);
    let created = call(
        &deps,
        &owner,
        "create_collection",
        json!({"name":"Scoped folder"}),
    )
    .await
    .unwrap();
    let id = created["structuredContent"]["collection"]["id"]
        .as_str()
        .unwrap();
    assert!(
        created["content"][0]["text"]
            .as_str()
            .unwrap()
            .starts_with("{\"collection\":{\"id\":")
    );
    for (tool, args, scope) in [
        ("list_collections", json!({}), "artifacts:read"),
        ("get_collection", json!({"id":id}), "artifacts:read"),
        (
            "create_collection",
            json!({"name":"Denied"}),
            "artifacts:publish",
        ),
        (
            "update_collection",
            json!({"id":id,"name":"Denied"}),
            "artifacts:publish",
        ),
        ("delete_collection", json!({"id":id}), "artifacts:publish"),
        (
            "add_artifacts_to_collection",
            json!({"id":id,"artifact_ids":[]}),
            "artifacts:publish",
        ),
        (
            "remove_artifacts_from_collection",
            json!({"id":id,"artifact_ids":[]}),
            "artifacts:publish",
        ),
    ] {
        for auth in [oauth(&[]), oauth(&["artifacts:delete"])] {
            assert_eq!(
                call(&deps, &auth, tool, args.clone()).await.unwrap_err(),
                McpError::Tool(AppError::Forbidden(format!(
                    "Missing required scope: {scope}"
                )))
            );
        }
    }
    let mut reader = oauth(&["artifacts:read", "artifacts:publish"]);
    reader.role = "reader".into();
    let inspected = call(&deps, &reader, "get_collection", json!({"id":id}))
        .await
        .unwrap();
    assert_eq!(
        inspected["structuredContent"]["collection"]["editable"],
        false
    );
    let listed = call(&deps, &reader, "list_collections", json!({}))
        .await
        .unwrap();
    assert_eq!(
        listed["structuredContent"]["collections"][0]["editable"],
        false
    );
    assert_eq!(
        call(&deps, &reader, "delete_collection", json!({"id":id}))
            .await
            .unwrap_err(),
        McpError::Tool(AppError::Forbidden(
            "Permission denied: reader keys cannot manage collections".into()
        ))
    );
    call(&deps, &owner, "delete_collection", json!({"id":id}))
        .await
        .unwrap();
}

#[tokio::test]
async fn folder_principals_separate_issuer_and_auth_kind_and_survive_secret_rotation() {
    let (_dir, deps) = fixture();
    let owner = oauth(&["artifacts:publish"]);
    let folder = call(
        &deps,
        &owner,
        "create_collection",
        json!({"name":"Principal folder"}),
    )
    .await
    .unwrap();
    let id = folder["structuredContent"]["collection"]["id"]
        .as_str()
        .unwrap();
    let mut other_issuer = deps.clone();
    let mut config = (*deps.config).clone();
    config.oauth.issuer = "https://other-issuer.example.test".into();
    other_issuer.config = Arc::new(config);
    let mut api_key = owner.clone();
    api_key.scopes = None;
    for (target, auth) in [(&other_issuer, &owner), (&deps, &api_key)] {
        assert_eq!(
            call(
                target,
                auth,
                "update_collection",
                json!({"id":id,"name":"Denied"})
            )
            .await
            .unwrap_err(),
            McpError::Tool(AppError::Forbidden(
                "Only the collection creator or an administrator can edit this collection".into()
            ))
        );
    }
    let mut rotated = owner.clone();
    rotated.label = "rotated credential".into();
    let updated = call(
        &deps,
        &rotated,
        "update_collection",
        json!({"id":id,"description":"retained owner"}),
    )
    .await
    .unwrap();
    assert_eq!(
        updated["structuredContent"]["collection"]["description"],
        "retained owner"
    );
    let conn = deps.data.pool.as_ref().unwrap().get().unwrap();
    let principal: (String, String) = conn
        .query_row(
            "SELECT created_by_kind,created_by FROM collections WHERE id=?1",
            [id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        principal,
        (
            "oauth".into(),
            "[\"https://issuer.example.test\",\"scope-client\"]".into()
        )
    );
    assert_eq!(
        conn.query_row("SELECT COUNT(*) FROM gallery_preferences", [], |row| row
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn metadata_outputs_redact_members_and_cover_when_read_access_narrows() {
    let (_dir, deps) = fixture();
    let mut owner = oauth(&["artifacts:publish", "artifacts:read"]);
    {
        let conn = deps.data.pool.as_ref().unwrap().get().unwrap();
        conn.execute("INSERT INTO artifacts(id,client_id,org,title,hidden) VALUES ('hidden-member','other-client','acme','Hidden',1)", []).unwrap();
    }
    let folder = call(&deps, &owner, "create_collection", json!({"name":"Redacted","artifact_ids":["hidden-member"],"cover_artifact_id":"hidden-member"})).await.unwrap();
    let id = folder["structuredContent"]["collection"]["id"]
        .as_str()
        .unwrap();
    assert_eq!(
        folder["structuredContent"]["collection"]["artifact_count"],
        1
    );
    owner.role = "author".into();
    for collection in [
        call(
            &deps,
            &owner,
            "update_collection",
            json!({"id":id,"description":"metadata only"}),
        )
        .await
        .unwrap()["structuredContent"]["collection"]
            .clone(),
        call(&deps, &owner, "list_collections", json!({}))
            .await
            .unwrap()["structuredContent"]["collections"][0]
            .clone(),
    ] {
        assert_eq!(collection["artifact_count"], 0);
        assert_eq!(collection["cover_artifact_id"], "");
    }
    let cleared = call(
        &deps,
        &owner,
        "update_collection",
        json!({"id":id,"cover_artifact_id":""}),
    )
    .await
    .unwrap();
    assert_eq!(
        cleared["structuredContent"]["collection"]["cover_artifact_id"],
        ""
    );
}
