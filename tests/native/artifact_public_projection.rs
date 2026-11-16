//! Byte characterization for modern metadata not frozen by the legacy conformance cases.

use std::sync::Arc;

use artifact_mcp::{
    config::AppConfig,
    mcp::{
        dispatch::{ProtocolEra, dispatch_for_era},
        protocol::OrderedJson,
    },
    model::{ArtifactMeta, PublisherIdentity, Role},
    ports::testing::FakeArtifacts,
};
use serde_json::{Value, json};

async fn request(method: &str, mut params: Value, meta: ArtifactMeta) -> Value {
    let config = AppConfig {
        public_base_url: "https://artifacts.test".into(),
        ..AppConfig::defaults()
    };
    let mut deps = super::u18_admin_routes::collection_test_deps(config);
    let mut artifacts = FakeArtifacts::with(meta.clone());
    let listed = meta.clone();
    artifacts.scripts.list_for_publisher = Some(Box::new(move |_| Ok(vec![listed.clone()])));
    artifacts.seed(
        meta,
        Some(artifact_mcp::model::ArtifactFile {
            content: b"<h1>A</h1>".to_vec(),
            content_type: "text/html".into(),
        }),
    );
    artifacts.scripts.list_bundle_files =
        Some(Box::new(|_, _| Ok(Some(vec![("index.html".into(), 10)]))));
    artifacts.scripts.set_category = Some(Box::new(|artifact, category, _| {
        let mut meta = artifact.meta().clone();
        meta.category = category;
        Ok(meta)
    }));
    artifacts.scripts.set_hidden = Some(Box::new(|artifact, hidden, _| {
        let mut meta = artifact.meta().clone();
        meta.hidden = hidden;
        Ok(meta)
    }));
    artifacts.scripts.restore = Some(Box::new(|artifact, restored_from, _, _| {
        let mut meta = artifact.meta().clone();
        meta.revision += 1;
        Ok(artifact_mcp::model::RestoreArtifactResult {
            meta,
            restored_from,
        })
    }));
    artifacts.scripts.list_revisions = Some(Box::new(|_| {
        Ok(artifact_mcp::model::RevisionHistory {
            current: 1,
            revisions: vec![],
        })
    }));
    deps.artifacts = Arc::new(artifacts);
    let auth = PublisherIdentity {
        client_id: "publisher".into(),
        org: "acme".into(),
        label: "Agent".into(),
        role: Role::Author,
        scopes: None,
    };
    params["_meta"]["io.modelcontextprotocol/protocolVersion"] = json!("2026-07-28");
    params["_meta"]["io.modelcontextprotocol/clientInfo"] =
        json!({"name":"projection-test","version":"1"});
    if params["_meta"]
        .get("io.modelcontextprotocol/clientCapabilities")
        .is_none()
    {
        params["_meta"]["io.modelcontextprotocol/clientCapabilities"] = json!({});
    }
    let message: OrderedJson = serde_json::from_value(json!({
        "jsonrpc":"2.0", "id":1, "method":method, "params":params
    }))
    .unwrap();
    dispatch_for_era(&message, &auth, &deps, ProtocolEra::Modern)
        .await
        .unwrap()
}

#[tokio::test]
async fn public_projection_review_bytes_and_title_fallback() {
    let mut meta = ArtifactMeta::fixture("a", "acme");
    meta.title = "  ".into();
    meta.owner_email = Some("private@example.test".into());
    meta.body_sha256 = "private-digest".into();
    meta.uploader_label = "Agent".into();
    let result = request(
        "tools/call",
        json!({
            "name":"list_artifacts", "arguments":{},
            "_meta":{"io.modelcontextprotocol/clientCapabilities":{"extensions":{
                "io.modelcontextprotocol/ui":{"mimeTypes":["text/html;profile=mcp-app"]}
            }}}
        }),
        meta,
    )
    .await;
    assert_eq!(
        result["_meta"]["com.agentshelf.artifact-mcp/review"].to_string(),
        r#"{"artifacts":[{"canFeedback":true,"canManage":true,"category":"","description":"","hidden":false,"id":"a","isBundle":false,"org":"acme","publisher":"Agent","revision":1,"thumbnailResourceUri":"artifact://a/thumbnail","title":"a","url":"https://artifacts.test/a"}]}"#
    );
}

#[tokio::test]
async fn public_projection_resource_read_bytes() {
    let meta = ArtifactMeta::fixture("a", "acme");
    let result = request(
        "resources/read",
        json!({"uri":"artifact://a"}),
        meta.clone(),
    )
    .await;
    assert_eq!(
        result.to_string(),
        r#"{"cacheScope":"private","contents":[{"_meta":{"com.agentshelf.artifact-mcp/bytes":10,"com.agentshelf.artifact-mcp/org":"acme","com.agentshelf.artifact-mcp/revision":1},"mimeType":"text/html","text":"<h1>A</h1>","uri":"artifact://a"}],"ttlMs":30000}"#
    );
    let mut bundle = meta;
    bundle.is_bundle = true;
    bundle.entry = "index.html".into();
    bundle.bytes = 10;
    let result = request("resources/read", json!({"uri":"artifact://a"}), bundle).await;
    assert_eq!(
        result.to_string(),
        r#"{"cacheScope":"private","contents":[{"_meta":{"com.agentshelf.artifact-mcp/bytes":98,"com.agentshelf.artifact-mcp/org":"acme","com.agentshelf.artifact-mcp/revision":1},"mimeType":"application/vnd.artifact-mcp.bundle+json","text":"{\"bytes\":10,\"entry\":\"index.html\",\"files\":[{\"bytes\":10,\"path\":\"index.html\"}],\"id\":\"a\",\"revision\":1}","uri":"artifact://a"}],"ttlMs":30000}"#
    );
}

#[tokio::test]
async fn public_projection_mutation_and_history_text_bytes() {
    for (name, args, expected) in [
        (
            "set_category",
            json!({"id":"a","category":""}),
            r#"{"id":"a","category":""}"#,
        ),
        (
            "set_visibility",
            json!({"id":"a","hidden":true}),
            r#"{"id":"a","hidden":true}"#,
        ),
        (
            "restore_artifact",
            json!({"id":"a","revision":1}),
            r#"{"id":"a","url":"https://artifacts.test/a","revision":2,"restoredFrom":1,"bytes":0}"#,
        ),
        (
            "list_revisions",
            json!({"id":"a"}),
            r#"{"id":"a","current":1,"revisions":[]}"#,
        ),
    ] {
        let result = request(
            "tools/call",
            json!({"name":name,"arguments":args}),
            ArtifactMeta::fixture("a", "acme"),
        )
        .await;
        assert_eq!(
            result["content"][0]["text"].as_str().unwrap(),
            expected,
            "{name}"
        );
    }
}
