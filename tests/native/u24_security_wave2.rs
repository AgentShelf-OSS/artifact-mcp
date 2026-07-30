//! Regression coverage for the coordinated Node/Rust security wave 2 changes.

use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    sync::Arc,
    sync::atomic::{AtomicU64, Ordering},
};

use artifact_mcp::config::{AppConfig, Secret, SeedKeys};
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
    fn new(label: &str) -> Self {
        let sequence = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "artifact-mcp-u24-{label}-{}-{sequence}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).expect("create u24 temp directory");
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
    let mut config = AppConfig {
        data_dir: data_dir.to_path_buf(),
        audit_ledger_hmac_key: Some(Secret::new("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=")),
        public_base_url: "http://conformance.test".to_owned(),
        seed_keys: SeedKeys::parse("administrator:admin:admin-secret"),
        ..AppConfig::defaults()
    };
    config.access.trust_headers = true;
    config.access.header_trust_allow_insecure = true;
    config.access.admin_emails = BTreeSet::from(["admin@example.test".to_owned()]);
    config
}

fn publisher_config_for(data_dir: &Path) -> AppConfig {
    let mut config = config_for(data_dir);
    config.seed_keys = SeedKeys::parse(
        "administrator:admin:admin-secret,owner:acme:owner-secret,intruder:other:foreign-secret",
    );
    config
}

async fn response_json(response: axum::response::Response) -> Value {
    let bytes = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read response body");
    serde_json::from_slice(&bytes).expect("response is JSON")
}

async fn create_org(router: &Router, name: &str) {
    create_org_with_domain(router, name, None).await;
}

async fn create_org_with_domain(router: &Router, name: &str, domain: Option<&str>) {
    let mut payload = json!({ "name": name });
    if let Some(domain) = domain {
        payload["domain"] = Value::String(domain.to_owned());
    }
    let response = router
        .clone()
        .oneshot(
            Request::post("/settings/orgs")
                .header("cf-access-authenticated-user-email", "admin@example.test")
                .header("x-artifact-mutation", "1")
                .header("sec-fetch-site", "same-origin")
                .header("content-type", "application/json")
                .body(Body::from(payload.to_string()))
                .expect("create-org request"),
        )
        .await
        .expect("create-org response");
    assert_eq!(response.status(), 200);
}

#[tokio::test]
async fn publisher_share_and_feedback_mutations_conceal_foreign_references() {
    let temp = TempDir::new("publisher-adjacent-concealment");
    runtime::run_with_bind(
        publisher_config_for(temp.path()),
        Arc::new(Observer),
        |_host, _port, router| async move {
            create_org_with_domain(&router, "acme", Some("acme.test")).await;
            create_org(&router, "other").await;

            let published = call_mcp_as(
                &router,
                "owner-secret",
                1,
                "publish_artifact",
                json!({ "html": "<h1>Adjacent concealment</h1>" }),
            )
            .await;
            let id = published["result"]["structuredContent"]["id"]
                .as_str()
                .expect("published artifact id")
                .to_owned();

            let shared = call_mcp_as(
                &router,
                "owner-secret",
                2,
                "create_share",
                json!({ "id": id, "expires": "never" }),
            )
            .await;
            let token = shared["result"]["structuredContent"]["token"]
                .as_str()
                .expect("share token")
                .to_owned();
            let foreign_share = call_mcp_as(
                &router,
                "foreign-secret",
                51,
                "revoke_share",
                json!({ "token": token }),
            )
            .await;
            let owner_revoke = call_mcp_as(
                &router,
                "owner-secret",
                3,
                "revoke_share",
                json!({ "token": token }),
            )
            .await;
            assert_eq!(owner_revoke["result"]["structuredContent"]["revoked"], true);
            let missing_share = call_mcp_as(
                &router,
                "foreign-secret",
                51,
                "revoke_share",
                json!({ "token": token }),
            )
            .await;
            assert_eq!(foreign_share, missing_share);
            assert_eq!(
                missing_share["result"]["content"][0]["text"],
                "Unknown share"
            );

            let feedback_response = router
                .clone()
                .oneshot(
                    Request::post(format!("/{id}/feedback"))
                        .header("cf-access-authenticated-user-email", "reader@acme.test")
                        .header("x-artifact-mutation", "1")
                        .header("sec-fetch-site", "same-origin")
                        .header("content-type", "application/json")
                        .body(Body::from(json!({ "body": "Conceal me" }).to_string()))
                        .expect("feedback request"),
                )
                .await
                .expect("feedback response");
            assert_eq!(feedback_response.status(), 201);
            let feedback = response_json(feedback_response).await;
            let feedback_id = feedback["id"].as_str().expect("feedback id").to_owned();

            let mut foreign_feedback = Vec::new();
            for (request_id, name) in [(61, "resolve_feedback"), (62, "reopen_feedback")] {
                foreign_feedback.push(
                    call_mcp_as(
                        &router,
                        "foreign-secret",
                        request_id,
                        name,
                        json!({ "feedback_id": feedback_id }),
                    )
                    .await,
                );
            }

            let deleted = call_mcp_as(
                &router,
                "owner-secret",
                4,
                "delete_artifact",
                json!({ "id": id }),
            )
            .await;
            assert_eq!(deleted["result"]["structuredContent"]["deleted"], true);

            for ((request_id, name), foreign_response) in
                [(61, "resolve_feedback"), (62, "reopen_feedback")]
                    .into_iter()
                    .zip(foreign_feedback)
            {
                let missing = call_mcp_as(
                    &router,
                    "foreign-secret",
                    request_id,
                    name,
                    json!({ "feedback_id": feedback_id }),
                )
                .await;
                assert_eq!(foreign_response, missing, "{name} must conceal existence");
                assert_eq!(
                    missing["result"]["content"][0]["text"],
                    format!("Unknown feedback: {feedback_id}")
                );
            }
            Ok(())
        },
    )
    .await
    .expect("bootstrap with fake listener");
}

async fn call_mcp(router: &Router, request_id: u64, name: &str, arguments: Value) -> Value {
    call_mcp_as(router, "admin-secret", request_id, name, arguments).await
}

async fn call_mcp_as(
    router: &Router,
    key: &str,
    request_id: u64,
    name: &str,
    arguments: Value,
) -> Value {
    let response = router
        .clone()
        .oneshot(
            Request::post("/mcp")
                .header("authorization", format!("Bearer {key}"))
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "jsonrpc": "2.0",
                        "id": request_id,
                        "method": "tools/call",
                        "params": { "name": name, "arguments": arguments }
                    })
                    .to_string(),
                ))
                .expect("MCP request"),
        )
        .await
        .expect("MCP response");
    assert_eq!(response.status(), 200);
    response_json(response).await
}

#[tokio::test]
async fn publisher_write_tools_conceal_foreign_and_missing_artifacts() {
    let temp = TempDir::new("publisher-write-concealment");
    runtime::run_with_bind(
        publisher_config_for(temp.path()),
        Arc::new(Observer),
        |_host, _port, router| async move {
            create_org(&router, "acme").await;
            create_org(&router, "other").await;

            let published = call_mcp_as(
                &router,
                "owner-secret",
                1,
                "publish_artifact",
                json!({ "html": "<h1>Conceal me</h1>" }),
            )
            .await;
            let id = published["result"]["structuredContent"]["id"]
                .as_str()
                .expect("published artifact id")
                .to_owned();
            let probes = [
                ("delete_artifact", json!({ "id": id })),
                ("set_visibility", json!({ "id": id, "hidden": true })),
                ("set_category", json!({ "id": id, "category": "Secret" })),
                ("create_share", json!({ "id": id, "expires": "never" })),
                ("restore_artifact", json!({ "id": id, "revision": 1 })),
            ];

            let mut foreign = Vec::new();
            for (name, arguments) in &probes {
                foreign.push(
                    call_mcp_as(&router, "foreign-secret", 39, name, arguments.clone()).await,
                );
            }

            let deleted = call_mcp_as(
                &router,
                "owner-secret",
                2,
                "delete_artifact",
                json!({ "id": id }),
            )
            .await;
            assert_eq!(deleted["result"]["structuredContent"]["deleted"], true);

            for ((name, arguments), foreign_response) in probes.iter().zip(foreign) {
                let missing =
                    call_mcp_as(&router, "foreign-secret", 39, name, arguments.clone()).await;
                assert_eq!(foreign_response, missing, "{name} must conceal existence");
                assert_eq!(
                    missing["result"]["content"][0]["text"],
                    format!("Unknown artifact: {id}")
                );
            }
            Ok(())
        },
    )
    .await
    .expect("bootstrap with fake listener");
}

#[tokio::test]
async fn admin_publish_targets_are_normalized_and_must_exist() {
    let temp = TempDir::new("publish-org");
    runtime::run_with_bind(
        config_for(temp.path()),
        Arc::new(Observer),
        |_host, _port, router| async move {
            create_org(&router, "publish-target").await;

            let single = call_mcp(
                &router,
                1,
                "publish_artifact",
                json!({ "html": "<h1>Single</h1>", "org": "  PUBLISH-TARGET  " }),
            )
            .await;
            let bundle = call_mcp(
                &router,
                2,
                "publish_bundle",
                json!({
                    "files": { "index.html": "<h1>Bundle</h1>" },
                    "org": "Publish-Target"
                }),
            )
            .await;
            assert_eq!(single["result"]["structuredContent"]["org"], "publish-target");
            assert_eq!(bundle["result"]["structuredContent"]["org"], "publish-target");

            for (request_id, name, arguments) in [
                (
                    3,
                    "publish_artifact",
                    json!({ "html": "<h1>Missing</h1>", "org": "missing-target" }),
                ),
                (
                    4,
                    "publish_bundle",
                    json!({
                        "files": { "index.html": "<h1>Missing</h1>" },
                        "org": "missing-target"
                    }),
                ),
            ] {
                let rejected = call_mcp(&router, request_id, name, arguments).await;
                assert_eq!(rejected["result"]["isError"], true);
                assert_eq!(
                    rejected["result"]["content"][0]["text"],
                    "Unknown organization \"missing-target\". Create it in the Organizations section first."
                );
            }
            Ok(())
        },
    )
    .await
    .expect("bootstrap with fake listener");
}
