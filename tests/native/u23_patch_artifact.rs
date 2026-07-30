//! Production-backed coverage for PBI-037 without binding a listener.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    sync::atomic::{AtomicU64, Ordering},
};

use artifact_mcp::config::{AppConfig, Secret, SeedKeys, StorageLimits};
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
            "artifact-mcp-u23-patch-{}-{sequence}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).expect("create u23 temp directory");
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
        audit_ledger_hmac_key: Some(Secret::new("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=")),
        public_base_url: "http://conformance.test".to_owned(),
        seed_keys: SeedKeys::parse("publisher:acme:owner-secret"),
        storage: StorageLimits {
            max_artifact_bytes: 32,
            max_bundle_bytes: 64,
            ..StorageLimits::default()
        },
        ..AppConfig::defaults()
    }
}

async fn call(router: &Router, id: u64, name: &str, arguments: Value) -> Value {
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
                .header("authorization", "Bearer owner-secret")
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

fn tool_error(response: &Value) -> &str {
    response["result"]["content"][0]["text"]
        .as_str()
        .expect("tool error text")
}

#[tokio::test]
async fn patch_artifact_contract_is_atomic_utf8_safe_and_revision_guarded() {
    let temp = TempDir::new();
    runtime::run_with_bind(
        config_for(temp.path()),
        Arc::new(Observer),
        |_host, _port, router| async move {
            let single = call(
                &router,
                1,
                "publish_artifact",
                json!({ "html": "before unique after" }),
            )
            .await;
            let single_id = structured(&single)["id"]
                .as_str()
                .expect("single id")
                .to_owned();
            let patched = call(
                &router,
                2,
                "patch_artifact",
                json!({
                    "id": single_id,
                    "expected_revision": 1,
                    "edits": [{ "find": "unique", "replace": "changed" }]
                }),
            )
            .await;
            assert_eq!(
                structured(&patched),
                &json!({
                    "id": single_id,
                    "revision": 2,
                    "bytes_before": 19,
                    "bytes_after": 20,
                    "edits_applied": 1
                })
            );
            assert_eq!(
                patched["result"]["content"][0]["text"],
                format!(
                    "{{\"id\":\"{single_id}\",\"revision\":2,\"bytes_before\":19,\"bytes_after\":20,\"edits_applied\":1}}"
                )
            );
            let read = call(
                &router,
                3,
                "read_artifact",
                json!({ "id": single_id }),
            )
            .await;
            assert_eq!(structured(&read)["content"], "before changed after");
            let history = call(
                &router,
                4,
                "list_revisions",
                json!({ "id": single_id }),
            )
            .await;
            assert_eq!(structured(&history)["current"], 2);
            assert_eq!(structured(&history)["revisions"].as_array().map(Vec::len), Some(1));

            let stale = call(
                &router,
                5,
                "patch_artifact",
                json!({
                    "id": single_id,
                    "expected_revision": 1,
                    "edits": [{ "find": "changed", "replace": "stale" }]
                }),
            )
            .await;
            assert_eq!(
                tool_error(&stale),
                "Artifact changed during update; fetch its current revision and retry"
            );
            let unchanged = call(
                &router,
                6,
                "read_artifact",
                json!({ "id": single_id }),
            )
            .await;
            assert_eq!(structured(&unchanged)["content"], "before changed after");

            let ranged = call(
                &router,
                7,
                "publish_artifact",
                json!({ "html": "A🎉B---C" }),
            )
            .await;
            let ranged_id = structured(&ranged)["id"]
                .as_str()
                .expect("range id")
                .to_owned();
            let ranged_patch = call(
                &router,
                8,
                "patch_artifact",
                json!({
                    "id": ranged_id,
                    "expected_revision": 1,
                    "edits": [
                        { "offset": 9, "length": 1, "replace": "see" },
                        { "offset": 5, "length": 1, "replace": "bee" }
                    ]
                }),
            )
            .await;
            assert_eq!(structured(&ranged_patch)["revision"], 2);
            assert_eq!(structured(&ranged_patch)["edits_applied"], 2);
            let ranged_read = call(
                &router,
                9,
                "read_artifact",
                json!({ "id": ranged_id }),
            )
            .await;
            assert_eq!(structured(&ranged_read)["content"], "A🎉bee---see");

            let split = call(
                &router,
                10,
                "patch_artifact",
                json!({
                    "id": ranged_id,
                    "expected_revision": 2,
                    "edits": [{ "offset": 2, "length": 1, "replace": "x" }]
                }),
            )
            .await;
            assert_eq!(tool_error(&split), "edit 1 offset 2 is not a UTF-8 boundary");

            let ambiguous_source = call(
                &router,
                11,
                "publish_artifact",
                json!({ "html": "alpha beta alpha" }),
            )
            .await;
            let ambiguous_id = structured(&ambiguous_source)["id"]
                .as_str()
                .expect("ambiguous id")
                .to_owned();
            let zero = call(
                &router,
                12,
                "patch_artifact",
                json!({
                    "id": ambiguous_id,
                    "expected_revision": 1,
                    "edits": [{ "find": "gamma", "replace": "changed" }]
                }),
            )
            .await;
            assert_eq!(
                tool_error(&zero),
                "edit 1 find matched 0 times; expected exactly once"
            );
            let twice = call(
                &router,
                13,
                "patch_artifact",
                json!({
                    "id": ambiguous_id,
                    "expected_revision": 1,
                    "edits": [{ "find": "alpha", "replace": "changed" }]
                }),
            )
            .await;
            assert_eq!(
                tool_error(&twice),
                "edit 1 find matched 2 times; expected exactly once"
            );
            let ambiguous_history = call(
                &router,
                14,
                "list_revisions",
                json!({ "id": ambiguous_id }),
            )
            .await;
            assert_eq!(structured(&ambiguous_history)["current"], 1);
            assert_eq!(
                structured(&ambiguous_history)["revisions"],
                json!([])
            );

            let capped = call(
                &router,
                15,
                "publish_artifact",
                json!({ "html": "x".repeat(32) }),
            )
            .await;
            let capped_id = structured(&capped)["id"]
                .as_str()
                .expect("capped id")
                .to_owned();
            let oversized = call(
                &router,
                16,
                "patch_artifact",
                json!({
                    "id": capped_id,
                    "expected_revision": 1,
                    "edits": [{ "offset": 32, "length": 0, "replace": "y" }]
                }),
            )
            .await;
            assert_eq!(tool_error(&oversized), "html exceeds 32 bytes (got 33)");
            let capped_history = call(
                &router,
                17,
                "list_revisions",
                json!({ "id": capped_id }),
            )
            .await;
            assert_eq!(structured(&capped_history)["current"], 1);
            assert_eq!(structured(&capped_history)["revisions"], json!([]));

            let bundle = call(
                &router,
                18,
                "publish_bundle",
                json!({
                    "files": {
                        "index.html": "hello world",
                        "assets/note.txt": "untouched"
                    }
                }),
            )
            .await;
            let bundle_id = structured(&bundle)["id"]
                .as_str()
                .expect("bundle id")
                .to_owned();
            let bundle_patch = call(
                &router,
                19,
                "patch_artifact",
                json!({
                    "id": bundle_id,
                    "expected_revision": 1,
                    "path": "docs/../index.html",
                    "edits": [{ "find": "world", "replace": "Earth" }]
                }),
            )
            .await;
            assert_eq!(structured(&bundle_patch)["revision"], 2);
            assert_eq!(structured(&bundle_patch)["bytes_before"], 20);
            assert_eq!(structured(&bundle_patch)["bytes_after"], 20);
            let bundle_entry = call(
                &router,
                20,
                "read_artifact",
                json!({ "id": bundle_id, "path": "index.html" }),
            )
            .await;
            assert_eq!(structured(&bundle_entry)["content"], "hello Earth");
            let untouched = call(
                &router,
                21,
                "read_artifact",
                json!({ "id": bundle_id, "path": "assets/note.txt" }),
            )
            .await;
            assert_eq!(structured(&untouched)["content"], "untouched");
            let traversal = call(
                &router,
                22,
                "patch_artifact",
                json!({
                    "id": bundle_id,
                    "expected_revision": 2,
                    "path": "../index.html",
                    "edits": [{ "find": "Earth", "replace": "unsafe" }]
                }),
            )
            .await;
            assert_eq!(
                tool_error(&traversal),
                "Unknown bundle file: ../index.html"
            );
            let bundle_history = call(
                &router,
                23,
                "list_revisions",
                json!({ "id": bundle_id }),
            )
            .await;
            assert_eq!(structured(&bundle_history)["current"], 2);
            assert_eq!(
                structured(&bundle_history)["revisions"]
                    .as_array()
                    .map(Vec::len),
                Some(1)
            );

            let missing_revision = call(
                &router,
                24,
                "patch_artifact",
                json!({
                    "id": bundle_id,
                    "path": "index.html",
                    "edits": [{ "find": "Earth", "replace": "missing" }]
                }),
            )
            .await;
            assert_eq!(missing_revision["error"]["code"], -32602);
            assert!(
                missing_revision["error"]["message"]
                    .as_str()
                    .is_some_and(|message| message.contains("expected_revision is required"))
            );

            Ok(())
        },
    )
    .await
    .expect("production-backed patch_artifact exercise");
}
