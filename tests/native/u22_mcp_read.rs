//! Production-backed coverage for PBI-035 and PBI-036 without binding a listener.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    sync::atomic::{AtomicU64, Ordering},
};

use artifact_mcp::config::{AppConfig, SeedKeys};
use axum::{
    Router,
    body::{Body, to_bytes},
    http::Request,
};
use serde_json::{Value, json};
use tower::ServiceExt;

use super::u20_runtime::runtime;

static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let sequence = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "artifact-mcp-u22-read-{}-{sequence}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).expect("create u22 temp directory");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ignored = std::fs::remove_dir_all(&self.0);
    }
}

struct Observer;

impl runtime::StartupObserver for Observer {
    fn stage(&self, _stage: runtime::StartupStage) {}
}

fn config_for(data_dir: &Path) -> AppConfig {
    AppConfig {
        data_dir: data_dir.to_path_buf(),
        public_base_url: "http://conformance.test".to_owned(),
        seed_keys: SeedKeys::parse("publisher:acme:owner-secret,foreign:other:foreign-secret"),
        ..AppConfig::defaults()
    }
}

async fn call(router: &Router, key: &str, id: u64, name: &str, arguments: Value) -> Value {
    let body = json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "tools/call",
        "params": { "name": name, "arguments": arguments }
    });
    let response = router
        .clone()
        .oneshot(
            Request::post("/mcp")
                .header("authorization", format!("Bearer {key}"))
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .expect("MCP request"),
        )
        .await
        .expect("MCP response");
    assert_eq!(response.status(), 200);
    let bytes = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read MCP response");
    serde_json::from_slice(&bytes).expect("MCP JSON response")
}

fn structured(response: &Value) -> &Value {
    &response["result"]["structuredContent"]
}

#[tokio::test]
async fn enriched_listing_and_read_artifact_contract_work_on_production_adapters() {
    let temp = TempDir::new();
    runtime::run_with_bind(
        config_for(temp.path()),
        Arc::new(Observer),
        |_host, _port, router| async move {
            let single = call(
                &router,
                "owner-secret",
                1,
                "publish_artifact",
                json!({
                    "html": "A🎉B",
                    "title": "Readable",
                    "description": "Round trip",
                    "category": "Reports"
                }),
            )
            .await;
            let single_id = structured(&single)["id"]
                .as_str()
                .expect("published single id")
                .to_owned();
            call(
                &router,
                "owner-secret",
                2,
                "set_visibility",
                json!({ "id": single_id, "hidden": true }),
            )
            .await;

            let listed = call(&router, "owner-secret", 3, "list_artifacts", json!({})).await;
            let row = structured(&listed)["artifacts"]
                .as_array()
                .expect("artifact rows")
                .iter()
                .find(|row| row["id"] == single_id)
                .expect("published artifact is listed");
            assert_eq!(row["org"], "acme");
            assert_eq!(row["category"], "Reports");
            assert_eq!(row["revision"], 1);
            assert_eq!(row["bytes"], 6);
            assert_eq!(row["is_bundle"], 0);
            assert_eq!(row["entry"], "");
            assert_eq!(row["hidden"], 1);
            assert!(row["updated_at"].is_string());
            assert!(row.get("client_id").is_none());
            assert!(row.get("body_sha256").is_none());

            let current = call(
                &router,
                "owner-secret",
                4,
                "read_artifact",
                json!({ "id": single_id }),
            )
            .await;
            assert_eq!(
                structured(&current),
                &json!({
                    "id": single_id,
                    "org": "acme",
                    "is_bundle": false,
                    "revision": 1,
                    "content_type": "text/html; charset=utf-8",
                    "bytes_total": 6,
                    "offset": 0,
                    "bytes_returned": 6,
                    "truncated": false,
                    "content": "A🎉B"
                })
            );
            assert_eq!(
                current["result"]["content"][0]["text"],
                format!(
                    "{{\"id\":\"{single_id}\",\"org\":\"acme\",\"is_bundle\":false,\"revision\":1,\"content_type\":\"text/html; charset=utf-8\",\"bytes_total\":6,\"offset\":0,\"bytes_returned\":6,\"truncated\":false,\"content\":\"A🎉B\"}}"
                )
            );

            let boundary = call(
                &router,
                "owner-secret",
                5,
                "read_artifact",
                json!({ "id": single_id, "offset": 0, "limit": 4 }),
            )
            .await;
            assert_eq!(structured(&boundary)["offset"], 0);
            assert_eq!(structured(&boundary)["bytes_returned"], 1);
            assert_eq!(structured(&boundary)["truncated"], true);
            assert_eq!(structured(&boundary)["content"], "A");

            let inside_sequence = call(
                &router,
                "owner-secret",
                6,
                "read_artifact",
                json!({ "id": single_id, "offset": 2, "limit": 5 }),
            )
            .await;
            assert_eq!(structured(&inside_sequence)["offset"], 5);
            assert_eq!(structured(&inside_sequence)["bytes_returned"], 1);
            assert_eq!(structured(&inside_sequence)["content"], "B");

            call(
                &router,
                "owner-secret",
                7,
                "update_artifact",
                json!({ "id": single_id, "html": "second" }),
            )
            .await;
            let historical = call(
                &router,
                "owner-secret",
                8,
                "read_artifact",
                json!({ "id": single_id, "revision": 1 }),
            )
            .await;
            assert_eq!(structured(&historical)["revision"], 1);
            assert_eq!(structured(&historical)["content"], "A🎉B");

            let bundle = call(
                &router,
                "owner-secret",
                9,
                "publish_bundle",
                json!({
                    "files": {
                        "index.html": "<h1>Bundle</h1>",
                        "assets/note.txt": "hello 🎉"
                    }
                }),
            )
            .await;
            let bundle_id = structured(&bundle)["id"]
                .as_str()
                .expect("published bundle id")
                .to_owned();
            let bundle_listing = call(
                &router,
                "owner-secret",
                10,
                "read_artifact",
                json!({ "id": bundle_id }),
            )
            .await;
            assert_eq!(structured(&bundle_listing)["entry"], "index.html");
            assert_eq!(
                structured(&bundle_listing)["content_type"],
                "application/json"
            );
            assert_eq!(structured(&bundle_listing)["bytes_returned"], 0);
            assert_eq!(
                structured(&bundle_listing)["files"],
                json!([
                    { "path": "assets/note.txt", "bytes": 10, "entry": false },
                    { "path": "index.html", "bytes": 15, "entry": true }
                ])
            );

            let bundle_file = call(
                &router,
                "owner-secret",
                11,
                "read_artifact",
                json!({ "id": bundle_id, "path": "assets/note.txt" }),
            )
            .await;
            assert_eq!(structured(&bundle_file)["content"], "hello 🎉");
            assert_eq!(structured(&bundle_file)["bytes_total"], 10);
            assert_eq!(
                structured(&bundle_file)["content_type"],
                "text/plain; charset=utf-8"
            );

            let traversal = call(
                &router,
                "owner-secret",
                12,
                "read_artifact",
                json!({ "id": bundle_id, "path": "../index.html" }),
            )
            .await;
            assert_eq!(traversal["result"]["isError"], true);
            assert_eq!(
                traversal["result"]["content"][0]["text"],
                "Unknown bundle file: ../index.html"
            );

            let foreign = call(
                &router,
                "foreign-secret",
                13,
                "read_artifact",
                json!({ "id": single_id }),
            )
            .await;
            let missing = call(
                &router,
                "foreign-secret",
                14,
                "read_artifact",
                json!({ "id": "missing1" }),
            )
            .await;
            assert_eq!(foreign["result"]["isError"], true);
            assert_eq!(missing["result"]["isError"], true);
            assert_eq!(
                foreign["result"]["content"][0]["text"],
                format!("Unknown artifact: {single_id}")
            );
            assert_eq!(
                missing["result"]["content"][0]["text"],
                "Unknown artifact: missing1"
            );

            Ok(())
        },
    )
    .await
    .expect("production-backed MCP exercise");
}
